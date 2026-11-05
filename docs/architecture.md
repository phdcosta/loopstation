# Loopstation — Documento de Arquitetura

**Status:** em implementação — M0 concluído; M1/M3 funcionando (gravação, overdub, undo, crop,
speed, direção, buses); M4 com WSOLA em tempo real (pitch preservado). Ver §21.
**Plataforma-alvo:** Windows 10/11 x64, driver ASIO (fallback WASAPI)
**Linguagem:** Rust (edition 2021+), UI em `egui`
**Licença pretendida:** GPL-3.0-or-later (ver §20.1 — obrigatório por causa do Ableton Link)

---

## 1. Objetivo e escopo

Construir uma loopstation de software com **8 canais de loop** (projetada para N, padrão 8), **trilha guia** opcional, gravação em tempo real, sincronismo de rede via **Ableton Link** e **time-stretch com preservação de pitch**, para uso ao vivo por um único músico.

### 1.1 O que o sistema faz (v1)

| Capacidade | Detalhe |
|---|---|
| Canais de loop | 8 por padrão, até 16; gravação, play/stop, overdub em camadas com undo |
| Trilha guia | WAV/MP3/FLAC/OGG; define um bloco de N compassos, com repetições (ex.: 64 compassos × 2 = 128) |
| Transporte | relógio musical único, sample-accurate, quantum configurável, BPM 20–300 |
| Tempo | time-stretch em tempo real; BPM muda por usuário **ou** pela rede sem alterar pitch |
| Crop | janela não destrutiva de compassos: 8 → 2 compassos; o loop passa a ter 2 compassos e repete só o trecho |
| Direção | forward / reverse por canal |
| Velocidade | 0.5×, 1×, 2× (e outros divisores que mantenham o período em beats inteiros) |
| Ganho | volume por canal de loop **e** volume por bus virtual |
| Roteamento | canal → bus (A/B/…); bus → canal(ais) físico(s) da interface |
| MIDI | learn de CC/Note para qualquer parâmetro, feedback de estado, presets |
| Monitoramento visual | playhead de cada loop, compassos restantes, aviso de fim de loop, posição na guia |
| Persistência | projeto em pasta (JSON + WAV float32), autosave, reabertura fiel |

### 1.2 Não-objetivos da v1 (explicitamente fora)

- Edição de áudio com corte/cola por amostra, comping, automação de parâmetros por curva.
- Exportação de mix, bounce, mastering.
- Sincronismo de vídeo, OSC, Rewire, VST/AU hosting.
- Múltiplos músicos remotos trocando áudio (só tempo/fase via Link).
- Metrônomo com subdivisões complexas (v1: click por beat, com acento no 1).

### 1.3 Princípio arquitetural central

> **Existe um único relógio musical — o transporte — dentro do motor de áudio.**
> A UI não manda no playhead: ela envia comandos e lê snapshots. Toda decisão musical
> (início de gravação, fim de loop, crop, disparo, troca de BPM) é resolvida na grade
> de compassos/beats, nunca “no momento em que o botão foi clicado”.

Esse princípio é o que permite gravação quantizada, loops de tamanhos diferentes, crop
não destrutivo, sync de rede e time-stretch conviverem sem casos especiais na UI.

---

## 2. Decisões arquiteturais (resumo)

| ID | Decisão | Justificativa |
|---|---|---|
| **D1** | Transporte é o único mestre de tempo; UI e MIDI são clientes | evita dois relógios divergindo |
| **D2** | Time-stretch com preservação de pitch, não varispeed | requisito: BPM muda via Link e o pitch não pode mudar (ADR 0001) |
| **D3** | Stretch em duas camadas: RT (baixa latência) + re-render offline (qualidade) | Link muda BPM continuamente; render offline sozinho teria latência de 1 ciclo |
| **D4** | Crop encurta o **período** do loop; áudio original preservado | comportamento pedido: repete só o trecho, sem silêncio |
| **D5** | Loop sempre ancorado na grade global de compassos (bar 0 do transporte) | permite loops de 2, 4, 8 compassos convivendo (polimetria) |
| **D6** | Buses internos são estéreo; matriz final mapeia bus → canais físicos | simples, cobre “A→L, B→R” e sobra para 4/8 saídas |
| **D7** | Overdub = pilha de camadas imutáveis + mixdown sob demanda | undo trivial, sem cópia do buffer principal |
| **D8** | Comandos da UI → motor por fila lock-free com timestamp musical | sample-accurate, sem locks no callback |
| **D9** | Telemetria motor → UI por triple buffer a ~60 Hz | UI nunca bloqueia o áudio |
| **D10** | Tudo decodificado offline para RAM (float32 planar) | nada de decode no callback |
| **D11** | Crossfade de costura 5 ms (2–20 ms) em toda descontinuidade | evita estalo em seam, crop, punch-in/out, troca de ratio (M0 usa *seam fade*; ver ADR 0002) |
| **D12** | Tempo map piecewise-linear em vez de acumulador float | posição derivada, sem drift |
| **D13** | Tempo não muda no meio de uma tomada (v1) | gravação sob BPM variável exigiria tempo map por take; adiado |
| **D14** | Projeto = pasta com JSON + WAVs; nada de banco de dados | inspecionável, versionável, recuperável à mão |

---

## 3. Visão de alto nível

```
┌──────────────────────────────────────────────────────────────────────────┐
│ UI thread (egui / eframe)                                                │
│  timeline, waveforms, faders, crop handles, MIDI learn, transport        │
└───────┬───────────────────────────────────────────────▲──────────────────┘
        │ EngineCommand (fila SPSC, timestamp musical)  │ EngineSnapshot (triple buffer)
        ▼                                               │
┌───────────────────────────────────────────────────────┴──────────────────┐
│ Control thread ("maestro")                                               │
│  valida comandos, resolve quantização, agenda no próximo boundary,       │
│  mantém modelo autoritativo (canais, buses, guia), grava projeto          │
└───┬──────────────┬───────────────┬───────────────┬───────────────────────┘
    │              │               │               │
    │ (fila)       │ (fila)        │ (fila)        │ (snapshot)
    ▼              ▼               ▼               ▼
┌────────┐  ┌────────────┐  ┌───────────┐  ┌──────────────┐
│ Link   │  │ MIDI thread│  │ Workers   │  │ Audio callback│
│ thread │  │ (midir)    │  │ decode /  │  │ (RT, ASIO)    │
│        │  │            │  │ stretch / │  │               │
│        │  │            │  │ disk I/O  │  │  transporte   │
└────────┘  └────────────┘  └───────────┘  │  canais       │
                                           │  buses        │
                                           │  master       │
                                           └──────┬────────┘
                                                  ▼
                                          interface ASIO
```

### 3.1 Threads e responsabilidades

| Thread | Prioridade | Pode alocar? | Pode bloquear? | Responsabilidade |
|---|---|---|---|---|
| Audio callback | time-critical (ASIO) | **não** | **não** | transporte, mix, stretchers, gravação em ring buffer |
| Control (“maestro”) | normal-alta | sim | sim | modelo autoritativo, quantização, agendamento, undo, autosave |
| Link | normal | sim | sim | sessão Link, tempo/fase, start/stop |
| MIDI in | normal | sim | sim | parse, learn, mapeamento → comandos |
| Decode/IO workers | baixa | sim | sim | `symphonia`, resample, peaks, WAV write, re-render de stretch |
| UI | normal | sim | sim | desenho, input, snapshots |

Regra dura: **o callback nunca toca em `Mutex`, `Vec::push`, `println!`, `File`, `TcpStream`
ou qualquer coisa que possa bloquear.** Toda comunicação é por `rtrb` (SPSC), `triple_buffer`
e buffers pré-alocados.

---

## 4. Modelo temporal

### 4.1 Unidades

- **Amostra**: `u64` absoluto desde o início do motor (nunca reinicia).
- **Beat**: `f64`, posição musical; 4 beats por compasso por padrão (`beats_per_bar` configurável).
- **Bar**: derivado de beat (`beat / beats_per_bar`), mas guardado como inteiro para exibição.
- **Quantum**: grade de disparo/gravação, padrão 1 compasso; opções 1 beat, 1/2 compasso, 1, 2, 4 compassos.

### 4.2 Tempo map

O BPM não é um escalar solto: é uma lista de segmentos. Cada mudança de tempo (usuário,
Link, ou automação futura) cria um segmento.

```rust
pub struct TempoSegment {
    pub start_sample: u64,   // amostra em que o segmento começa
    pub start_beat: f64,     // beat correspondente (contínuo entre segmentos)
    pub bpm: f64,            // > 0
}

pub struct TempoMap {
    segments: Vec<TempoSegment>, // ordenado, último sempre no futuro
    sample_rate: f64,
    beats_per_bar: u32,
}

impl TempoMap {
    #[inline]
    fn samples_per_beat(&self, bpm: f64) -> f64 { self.sample_rate * 60.0 / bpm }

    pub fn beat_at(&self, sample: u64) -> f64 {
        let seg = self.segment_for(sample);
        seg.start_beat + (sample - seg.start_sample) as f64 / self.samples_per_beat(seg.bpm)
    }

    pub fn sample_at(&self, beat: f64) -> u64 { /* inverso, com clamp no início */ }

    /// Mudança de tempo agendada para um boundary musical (nunca "agora").
    pub fn push_change(&mut self, at_beat: f64, bpm: f64) -> u64 { /* ... */ }
}
```

Por que não `pos += bpm/60/sr` por amostra: acumula erro e torna impossível responder
“onde estará o beat 32 daqui a 3 compassos?” sem simulação. Com o tempo map, toda posição
é **derivada**, e o agendamento é exato.

### 4.3 Ancoragem (bar 0)

- O transporte tem um `anchor_sample` (início da reprodução). `bar 0` = `anchor_sample`.
- Todo loop tem `anchor_beat` (múltiplo de `beats_per_bar` a partir de 0).
- Fase de um loop: `phase = ((transport_beat - loop.anchor_beat).rem_euclid(loop.period_beats)) / loop.period_beats`.
- A guia também é ancorada em `anchor_beat = 0`, com `period_beats = guide_bars * beats_per_bar * repeats`.

Consequência: loops de 2 e de 8 compassos tocam juntos e se realinham a cada 8 compassos,
sem nenhum código especial. Crop e speed mudam `period_beats`, e a fase é recalculada.

### 4.4 Quantização

Toda ação tem um `Quantize` resolvido no control thread:

```rust
pub enum Quantize { Off, Beat, Bar, Bars(u32), Quantum }
```

`resolve(at: TransportPos, q: Quantize) -> f64` devolve o **beat** em que a ação deve ocorrer.
O comando viaja para o callback com esse beat; o callback compara com a posição do bloco
e executa no sample exato (ou agenda para o bloco seguinte).

### 4.5 Gravação quantizada (fluxo)

1. Usuário arma o canal (`Arm`) e aperta Record.
2. Control thread resolve o início: próximo múltiplo do quantum.
3. No sample exato, o callback começa a copiar a entrada para o ring buffer de gravação
   do canal, aplicando compensação de latência.
4. O fim é determinado **musicalmente**: o usuário aperta Stop, ou define `record_bars`
   antecipadamente. O fim é arredondado para o próximo boundary de compasso (ou beat,
   configurável).
5. `take.length_beats` é derivado do tempo map; `take.source_bpm` = BPM médio do período
   (v1: constante, ver D13).
6. O take vira uma **camada** do canal; o worker escreve o WAV em disco em paralelo.

---

## 5. Modelo de dados (núcleo)

```rust
// ---------- Identificadores ----------
pub struct ChannelId(pub u8);   // 0..=15
pub struct BusId(pub u8);       // 0..=7
pub struct LayerId(pub u32);

// ---------- Áudio ----------
/// Áudio decodificado, planar, na sample rate do motor.
pub struct AudioBuffer {
    pub channels: u8,          // 1 = mono, 2 = estéreo
    pub frames: u64,
    pub data: Vec<f32>,        // planar: ch0 inteiro, depois ch1
    pub sample_rate: u32,
}

/// Take gravado: uma camada imutável.
pub struct Take {
    pub id: LayerId,
    pub audio: Arc<AudioBuffer>,
    pub start_beat: f64,        // onde começa dentro do loop (normalmente 0)
    pub length_beats: f64,
    pub source_bpm: f64,
    pub punch_fade_in: u32,     // amostras
    pub punch_fade_out: u32,
}

// ---------- Canal ----------
pub enum ChannelState { Empty, Armed, Recording, Playing, Stopped, Overdubbing }

pub struct LoopChannel {
    pub id: ChannelId,
    pub layers: Vec<Take>,       // camada 0 = base; demais = overdubs
    pub state: ChannelState,
    pub anchor_beat: f64,
    pub content_beats: f64,      // duração musical do material (após crop)
    pub crop: CropWindow,        // ver §8.4
    pub speed: f32,              // 0.5, 1.0, 2.0 ...
    pub direction: Direction,    // Forward | Reverse
    pub gain_db: f32,
    pub pan: f32,                // -1..1 (só relevante se o bus for estéreo)
    pub bus: BusId,
    pub muted: bool,
    pub soloed: bool,
    pub input: InputSource,      // qual canal físico alimenta a gravação
    pub crossfade_ms: f32,       // padrão 5.0
}

impl LoopChannel {
    /// Período efetivo em beats: crop encolhe, speed encurta mais.
    pub fn period_beats(&self) -> f64 {
        (self.crop.length_beats() / self.speed as f64).max(1.0 / 16.0)
    }
}

// ---------- Crop ----------
pub struct CropWindow {
    pub start_beat: f64,   // offset dentro do material original
    pub length_beats: f64, // 0 = material inteiro
    pub total_beats: f64,  // material disponível (não destrutivo)
}
```

### 5.1 Buses

```rust
pub struct Bus {
    pub id: BusId,
    pub name: String,
    pub gain_db: f32,
    pub mute: bool,
    /// Para onde este bus vai na placa. Vazio = não roteado.
    pub outputs: Vec<OutputTarget>,
}

pub struct OutputTarget {
    pub device_channel: u16, // 0-based no device
    pub gain_db: f32,        // permite -inf (desligado) e +6
}

pub struct OutputMatrix {
    pub buses: Vec<Bus>,     // padrão: A, B
    pub master_gain_db: f32,
    pub limiter: bool,       // brickwall de segurança, -0.3 dBFS
}
```

Exemplo do cenário do usuário:

```rust
// canais 1..=2 -> bus A -> saída L
// canais 3..=8 -> bus B -> saída R
bus_a.outputs = vec![OutputTarget { device_channel: 0, gain_db: 0.0 }];
bus_b.outputs = vec![OutputTarget { device_channel: 1, gain_db: 0.0 }];
```

---

## 6. Motor de áudio em tempo real

### 6.1 Ordem de processamento por bloco

```
for each block of N frames:
  1. drenar fila de comandos até o beat do bloco (aplicar no offset de sample)
  2. atualizar transporte (tempo map -> beat/sample do bloco)
  3. atualizar ratios de stretch (rampa/one-pole, ~100 ms)
  4. para cada canal ativo: renderizar (stretcher) -> gain/pan -> somar no bus
  5. guia: renderizar (stretcher) -> gain -> bus da guia
  6. buses: gain -> matriz de saída -> canais físicos
  7. master: limiter (opcional) -> clip-safe
  8. gravação: copiar entrada compensada para os ring buffers dos canais armados
  9. publicar telemetria (a cada ~16 ms, não a cada bloco)
```

O passo 1 é o único lugar onde comandos são aplicados, e sempre em offsets de sample
dentro do bloco — daí o “sample-accurate”.

### 6.2 Fila de comandos

```rust
pub enum EngineCommand {
    Transport(TransportCmd),                 // play, stop, set_bpm, set_quantum
    Channel { id: ChannelId, cmd: ChannelCmd },
    Bus { id: BusId, cmd: BusCmd },
    Guide(GuideCmd),
    SetOutputMatrix(OutputMatrix),
    Calibrate(CalibrationCmd),
}

pub struct TimedCommand {
    pub at_beat: f64,      // -1.0 = "no próximo bloco"
    pub cmd: EngineCommand,
}

pub enum ChannelCmd {
    Arm, Disarm, RecordStart { bars: Option<u32> }, RecordStop,
    Play, Stop, Clear, UndoLayer, RedoLayers,
    SetCrop(CropWindow), SetSpeed(f32), SetDirection(Direction),
    SetGain(f32), SetPan(f32), SetBus(BusId), SetCrossfade(f32),
    SetLayerMix { layer: LayerId, gain_db: f32 },
}
```

- Capacidade fixa (ex.: 1024 comandos) alocada na inicialização.
- Se a fila enche, o control thread espera (backpressure) — nunca o callback.
- Comandos são `Copy`/`Clone` sem `String`; textos (nomes, paths) vivem no control thread
  e são referenciados por `Arc<ProjectState>` trocado por `arc-swap`.

### 6.3 Estado compartilhado

| Dado | Mecanismo | Frequência |
|---|---|---|
| Comandos | `rtrb::Producer/Consumer` | por evento |
| Telemetria | `triple_buffer` (snapshot `Copy`) | 60 Hz |
| Áudio de takes (imutável) | `Arc<AudioBuffer>` + `arc-swap` | na carga/commit |
| Amostras de entrada (gravação) | `rtrb` de blocos | contínuo |
| Estado do projeto (nomes, paths) | `arc-swap<ProjectView>` | raro |

### 6.4 Telemetria (snapshot)

```rust
#[derive(Clone, Copy)]
pub struct EngineSnapshot {
    pub transport: TransportView,      // bpm, playing, bar, beat, sample
    pub guide: Option<GuideView>,      // bar dentro do bloco, repetição, seção
    pub channels: [ChannelView; 16],
    pub buses: [BusView; 8],
    pub cpu_load: f32,
    pub xruns: u32,
    pub link: LinkView,                // peers, bpm da sessão, playing
}

#[derive(Clone, Copy)]
pub struct ChannelView {
    pub state: ChannelState,
    pub phase: f32,            // 0..1 dentro do período
    pub bars_remaining: f32,   // < 1.0 => "vai terminar"
    pub peak_l: f32,
    pub peak_r: f32,
    pub layer_count: u8,
    pub period_beats: f32,
    pub crop_start: f32,
    pub crop_len: f32,
}
```

O snapshot precisa ser `Copy` (sem `Vec`, sem `String`) para caber no triple buffer.

---

## 7. Time-stretch (o coração do sistema)

### 7.1 Requisitos

- R1: ratio variável em tempo real, faixa 0.25×–4× (na prática 0.5×–2×).
- R2: pitch preservado.
- R3: mudança de ratio **sem** desalinhar o loop da grade de compassos.
- R4: sem estalo na costura do loop (seam) nem na troca de ratio.
- R5: CPU compatível com 8 canais + guia a 48 kHz / 128 frames.
- R6: latência musical zero (o loop continua na grade; só a “textura” muda).

### 7.2 Por que duas camadas

| Camada | O que faz | Quando | Latência de resposta | Qualidade |
|---|---|---|---|---|
| **RT** | stretcher no callback, ratio suavizado | sempre | ~50–200 ms (smoothing) | boa |
| **Offline** | re-render do ciclo em worker, qualidade alta | quando o BPM fica estável ≥ 2 s | 1 ciclo (até o próximo boundary) | máxima |

O offline sozinho não serve: um loop de 8 compassos a 120 BPM tem 16 s, então a resposta
a uma mudança de BPM chegaria 16 s depois — inaceitável para Link. O RT sozinho serve,
mas em ratios extremos a qualidade cai; o re-render offline entra por cima e é trocado
no boundary com crossfade.

### 7.3 Geometria do stretch (crítico)

O erro clássico é deixar o stretcher “livre”: ele produz um número variável de amostras
e o loop deriva. Aqui o stretcher é **dirigido pela grade**:

```
period_samples(bpm) = period_beats * sr * 60 / bpm
phase              = ((transport_beat - anchor_beat) mod period_beats) / period_beats
input_beat         = crop.start_beat + phase * crop.length_beats        // forward
                   = crop.start_beat + crop.length_beats - phase * crop.length_beats // reverse
input_sample       = input_beat * sr * 60 / source_bpm
```

O callback pede ao stretcher **exatamente** `frames` de saída para o bloco, e informa a
posição de entrada desejada. O stretcher (WSOLA/phase-vocoder) escolhe os grãos de entrada
próximos dessa posição — ou seja, a fase é imposta pela timeline, não acumulada pelo
algoritmo. Na virada do ciclo, a posição de entrada salta de `crop.end` para `crop.start`
e o crossfade de seam cobre o salto.

Implementação de referência (contrato real, em `looper-stretch`):

```rust
/// Janela periódica do material: o crop [lo, lo+len) é lido como um anel.
pub struct LoopWindow { pub lo: f64, pub len: f64 }

pub trait Stretcher: Send {
    /// Preenche exatamente `out_l.len()` amostras.
    /// - `start`: posição de entrada (domínio `u`, frames) da primeira amostra
    /// - `ratio`: frames de material consumidos por frame de saída
    /// - `window`: janela periódica (crop) — leitura com interpolação no seam
    /// - `reverse`: espelha a leitura dentro da janela
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

    fn reset(&mut self);
    fn name(&self) -> &'static str;
    fn pitch_preserving(&self) -> bool { true }
}
```

O domínio `u` cresce sempre; no sentido inverso a janela espelha (`u = lo` é o fim do
material). Isso permite esticar nos dois sentidos com o mesmo algoritmo, sem pré-renderizar
o buffer revertido.

Backends:

1. **WSOLA** (`looper-stretch::WsolaStretcher`) — **implementado**. Sem dependência
   externa, determinístico, bom para material rítmico (preserva transientes melhor que
   phase vocoder). Frame 1024 / hop 512 / busca ±384 a 48 kHz.
2. **Signalsmith Stretch** (MIT, C++) ou **Rubber Band R3** (GPL) — candidatos para
   qualidade máxima quando o re-render offline do §7.2 for implementado.
3. **SoundTouch** (LGPL) — fallback.
4. **Varispeed** — modo fita opcional (não é o padrão).

O trait existe para que o backend seja trocável sem tocar no motor, e para permitir um
backend determinístico nos testes offline.

### 7.6 WSOLA dirigido pela grade (implementado)

O stretcher **não escolhe** onde está o loop: a posição nominal de cada grão vem da
timeline (`start + n·ratio`, com `start` recalculado a cada bloco a partir da fase do
transporte). Assim, mesmo com BPM variável (Link), a fase não acumula deriva: o grão é
buscado perto da posição da grade, não perto da posição anterior somada ao ratio.

Por hop (512 amostras de saída):

1. `nominal = start + offset_saída · ratio`.
2. Busca hierárquica (passo 8, depois passo 1) do deslocamento em `±384` amostras que
   maximiza a correlação normalizada entre a **continuação natural** do grão anterior
   (`prev + hop`) e o candidato.
3. Sobreposição: `out = w_tail·material(prev+hop+m) + w_head·material(candidato+m)`,
   com metades de Hann complementares (`w_head + w_tail = 1`), o que mantém a amplitude
   constante em material correlacionado.
4. `prev = candidato`.

Garantias cobertas por teste:

- **Pitch**: senoide de 220 Hz a 2× continua em ~220 Hz (modo fita daria 440 Hz).
  O mesmo para 0,5×.
- **Independência de bloco**: renderizar com blocos de 128 ou 256 produz o mesmo sinal
  (o estado é por hop, não por bloco) — essencial porque o motor fatia blocos em comandos.
- **Reset determinístico**: após `reset()`, o mesmo trecho é reproduzido.
- **Silêncio entra, silêncio sai**; canais estéreo independentes.

Em `ratio ≈ 1` o motor usa leitura direta periódica (bit-exata e mais barata) e só liga o
WSOLA quando há stretch de fato. Isso mantém a reprodução normal transparente.

Custo: ~10 k operações por hop por canal (≈ 1 M ops/s por canal a 48 kHz), folgado para
8 canais + guia.

### 7.4 Custo e orçamento

Estimativa conservadora a 48 kHz, bloco 128, 8 canais estéreo + guia estéreo:

| Item | CPU (1 core moderno) |
|---|---|
| 8 × stretcher estéreo | 20–45% |
| Mix + buses + matriz | 3–5% |
| Gravação (cópia + ring) | 1–2% |
| Guia (stretcher) | 3–6% |
| **Total** | **~30–60% de 1 core** |

Se estourar: (a) reduzir canais ativos, (b) usar o cache offline e desligar o RT quando
o BPM está estável, (c) rodar stretchers em blocos maiores (256/512) mantendo a I/O em 128.

### 7.5 Casos de borda

- **Ratio muda durante o ciclo**: o ratio é suavizado (one-pole, τ ≈ 100 ms), mas o
  *período* muda no próximo boundary (senão o loop “escorrega”). Entre o boundary antigo
  e o novo, o trecho final é levemente comprimido/esticado para fechar exato.
- **Ratio extremo (> 2×)**: aumenta artefato; UI avisa e limita a 2× por padrão.
- **Reverse**: o `LoopWindow` espelha a leitura (`at(u, reverse)`), então o WSOLA trabalha
  no mesmo domínio crescente e não precisa de buffer revertido. A troca de direção no play
  reinicia o estado do stretcher e soa dentro do fade de seam.
- **Guia longa (192 compassos)**: fica inteira em RAM (ver §19); o stretcher processa
  por streaming a partir da posição da timeline, não precisa de buffer de saída gigante.
- **MP3**: decoder delay/padding pode deslocar a guia alguns ms. Preferir WAV/FLAC para
  guia; se MP3, aplicar correção de gapless quando o decoder informar.

---

## 8. Canais de loop

### 8.1 Máquina de estados

```
        Arm                RecordStart (no boundary)
Empty ────────► Armed ─────────────────────────────► Recording
  ▲               │                                     │
  │               │ Disarm                              │ RecordStop (no boundary)
  │               ▼                                     ▼
  └──────────── Empty ◄── Clear ◄────────────────── Playing ◄──► Stopped
                                                    │  ▲
                                       Overdub ─────┘  └───── Undo/Redo
```

Transições só acontecem em boundaries quantizados, exceto `Stop` imediato (com fade-out
de 5 ms) e `Clear` (fade-out de 10 ms).

### 8.2 Gravação

**Tamanho definido antes (`record_bars`)** — é o fluxo principal:

1. o usuário escolhe N compassos para o canal (`SetRecordBars(n)`, `0` = livre);
2. arma; a tomada começa no boundary do quantum (cabeça do compasso);
3. no início da tomada o motor **reserva** `N * beats_per_bar * samples_per_beat`
   amostras em `rec_buf` e agenda `FinishRecord` para `âncora + N compassos`;
4. no compasso seguinte a tomada fecha **no sample exato** e o canal passa a `Playing` no
   mesmo instante: o loop já entra tocando, sem gap e sem comando manual.

Como o fim é um comando agendado (`submit_at`), o motor fatia o bloco nesse sample — a
transição gravação → play é sample-accurate. O `anchor_beat` da tomada é o beat do início,
então a fase 0 do loop coincide com a cabeça do compasso em que a gravação começou.

Um `REC` manual durante uma tomada de tamanho fixo **aborta**: o fim automático é
cancelado e a tomada fecha no próximo boundary do quantum (loop com duração menor). Um
`FinishRecord` atrasado nunca remexe um canal que já está tocando (guarda de estado).

`Clear` também **remove as transições pendentes** do canal (`StartRecord`, `FinishRecord`,
`StartOverdub`, `FinishOverdub`). Sem isso, limpar durante uma tomada deixava o fim
automático antigo na fila e ele fechava a tomada seguinte no meio — o loop ficava com a
duração certa, mas a segunda metade em silêncio (sintoma: "pedi 2 compassos, gravei 1").
Regressão coberta por `clearing_during_a_take_does_not_shorten_the_next_take`.

O **fade de costura** é por canal (`SetCrossfadeMs`, 0–20 ms, padrão 5 ms). Para loops com
ataque no tempo forte o certo é 0–2 ms; material contínuo pede 5–10 ms. Quando o *material
gravado* já começa mais baixo que o corpo da tomada, a UI avisa — nesse caso o fade é do
captura (AGC do Windows/microfone), não do motor.

Outros pontos:

- Entrada: mono ou estéreo, escolhida por canal (`InputSource`), com ganho de entrada
  (§14.4).
- Monitoramento: opcional (pode gerar realimentação; padrão desligado em ASIO).
- Compensação de latência: o take é deslocado por `round_trip_samples` (§14).
- Punch-in/out: fades de 2–5 ms nas bordas para não estalar.
- Durante a gravação, o BPM é travado (D13). Se o Link pedir mudança, ela é agendada
  para o fim da tomada e a UI mostra “tempo pendente”.

### 8.3 Overdub

Modelo de camadas (D7):

```
layers = [ base, od1, od2, ... ]
mix = Σ layers[i] * layer_gain[i]   (com fades de punch)
```

- Overdub grava uma nova camada sobre o loop tocando; o músico ouve o mix atual.
- `UndoLayer` remove a última camada (não destrutivo, instantâneo).
- `Commit` (opcional) faz mixdown para uma camada só, liberando RAM; guarda o histórico
  em disco para undo posterior.
- Limite padrão: 8 camadas por canal (configurável); aviso de RAM.

### 8.4 Crop (não destrutivo)

Semântica (D4), exatamente como pedido:

- O material tem `total_beats` (ex.: 8 compassos = 32 beats).
- `crop = { start_beat, length_beats }`, com `start` e `length` em múltiplos de beat
  (UI oferece snap em compasso).
- O período do canal passa a ser `length_beats / speed`; o loop **repete só o trecho**.
- Aumentar o crop de volta restaura o material — nada é apagado.
- Crop pode ser feito com o loop tocando: aplica-se no próximo boundary do ciclo antigo,
  com crossfade, e o novo período entra em vigor dali em diante.
- Crop + speed: `period_beats = length_beats / speed` deve ser múltiplo de 1 beat
  (idealmente de 1 compasso). A UI restringe as combinações válidas e mostra o período
  resultante em compassos.

Exemplo:

| Material | Crop | Speed | Período resultante |
|---|---|---|---|
| 8 compassos (32 beats) | 0..8 beats (2 compassos) | 1× | 2 compassos |
| 8 compassos | 0..16 beats (4 compassos) | 2× | 2 compassos |
| 8 compassos | 4..20 beats (4 compassos) | 1× | 4 compassos (começa no compasso 2) |

### 8.5 Direção e velocidade

- `Direction::Reverse`: buffer revertido pré-computado; `phase` invertida na fórmula de §7.3.
- `Speed`: multiplicador de taxa **com pitch preservado** (stretch). O período musical
  encolhe na proporção inversa — 8 compassos a 2× ocupam 4 compassos. É o comportamento
  coerente com “loop sincronizado”: a música continua na grade.
- Modo “fita” (varispeed) é um toggle por canal, para quem quiser o efeito de pitch subindo;
  nesse caso o período não muda e o pitch acompanha. Fora do escopo de aceitação do M3.

> **Estado no M0:** a geometria (`period_beats`, fase, crop × speed) já é a definitiva e
> está coberta por testes. O áudio ainda usa `LinearStretcher` (leitura 1:1), portanto o
> pitch acompanha a velocidade; o backend com preservação de pitch entra no M4. Ver
> [`docs/adr/0001-speed-2x-semantics.md`](adr/0001-speed-2x-semantics.md).

### 8.6.1 Cola da emenda (tomada recém-gravada)

A reserva de latência dá uma folga natural para fechar a emenda do loop. A tomada grava
`material + comp + cola`; no fim:

- descarta `comp` do começo (latência);
- o material fica exatamente N compassos;
- a **cola** (`cola emenda`, 0–200 ms, padrão 30 ms) é o áudio gravado logo **depois** do
  fim do material, que continuaria naturalmente depois do último frame;
- esse trecho entra no começo do loop com crossfade linear: `take[i] = cauda[i]·(1−x) +
  take[i]·x`, com `x` subindo de 0 a 1 ao longo da cola.

Resultado: o último frame do loop "vira" o primeiro sem salto, porque o começo passa a ser
a continuação real do fim. A reserva total é `comp + cola`. O `fade costura` (5 ms) continua
cuidando do clique de borda; a cola cuida da **descontinuidade musical** da emenda.

Teste: `seam_glue_crossfades_the_loop_joint` (senoide que não fecha no compasso: o salto
fim→começo cai para menos de 60% com 60 ms de cola).

### 8.6 Crossfade de costura

- Janela padrão 5 ms, configurável 2–20 ms, curva equal-power.
- Aplicado em: seam do loop, bordas do crop, punch-in/out, troca de direção/speed,
  troca de ratio no boundary, transição RT→offline cache.
- Implementação: crossfade **no domínio do tempo**, entre a cauda do ciclo anterior e o
  início do novo, ambos lidos do mesmo material — evita descontinuidade de fase.
- Nunca aplicar crossfade em material percussivo curto sem teste auditivo: 5 ms pode
  comer o transiente. Por isso é configurável por canal.

> **Estado no M0:** o motor implementa um *seam fade* equal-power (fade-in/fade-out,
> 5 ms) — zera a amplitude no seam sem deslocar fase. O crossfade sobreposto descrito
> acima depende do backend de stretch e entra no M4. Ver
> [`docs/adr/0002-seam-fade.md`](adr/0002-seam-fade.md).

### 8.7 Nivelamento de tomada (auto-normalize)

Microfone interno de notebook entrega tipicamente **-50 a -70 dBFS**. A tomada é gravada
corretamente, mas o loop "toca" inaudível — o músico conclui que não gravou nada.

No fim da tomada o motor mede o pico e, se ele ficar **abaixo de -40 dBFS** (claramente um
problema de ganho, não de dinâmica), aplica um ganho de normalização até **-6 dBFS**, com
teto de **+42 dB**. O ganho aplicado vai para a telemetria (`take_gain_db`) e a UI mostra
"tomada normalizada +34 dB"; o undo guarda a tomada crua. O caminho preferido continua
sendo acertar o **trim de entrada** no rodapé antes de gravar (§14.4).

### 8.8 Loops importados e alinhamento à grade

Arquivos raramente têm exatamente N compassos: um loop de 1,85 compassos (pela duração e
pelo BPM corrente) precisa ser **esticado para 2 compassos** — senão sai de sincronia com
os loops gravados, que estão presos à grade.

No import o app calcula `compassos = round(duração × BPM / 60 / beats_por_compasso)`,
manda um comprimento-alvo em beats (`load_loop_to_beats`) e o motor recalcula o BPM de
origem para que o arquivo caiba exatamente nesse número de beats:

```
source_bpm = target_beats × 60 / duração
ratio      = duração / (target_beats × spb(BPM do transporte))
```

Como o período do canal passa a ser um número inteiro de compassos, o loop entra na cabeça
do compasso e permanece sincronizado para sempre. O WSOLA compensa o `ratio` sem mudar o
pitch; a UI informa a correção ("1,85 compassos → 2 compassos, stretch -7,5%").

---

## 9. Trilha guia

### 9.1 Conceito

A guia é um **bloco musical**: um áudio com `bars` compassos, tocado `repeats` vezes,
totalizando `total_bars = bars * repeats`. Ex.: 64 compassos × 2 = 128; ou 192 × 1.
Ela define a “forma” sobre a qual os loops são montados, mas **não é o relógio** — quem
manda no tempo é o transporte (D1), e a guia se adapta a ele por stretch.

```rust
pub struct GuideTrack {
    pub audio: Arc<AudioBuffer>,   // já na SR do motor
    pub file_bpm: f64,             // BPM de referência do arquivo (informado/estimado)
    pub beats_per_bar: u32,        // normalmente 4
    pub bars: u32,                 // 64
    pub repeats: u32,              // 2
    pub sections: Vec<GuideSection>, // opcional: intro, A, B, solo...
    pub gain_db: f32,
    pub bus: BusId,
    pub loop: bool,                // repete indefinidamente ou para no fim
    pub count_in_bars: u32,        // 0..8
}

pub struct GuideSection {
    pub name: String,
    pub start_bar: u32,
    pub bars: u32,
    pub color: [u8; 3],
}
```

### 9.2 Sincronismo

- `guide_period_beats = bars * beats_per_bar` (um ciclo do bloco).
- Posição dentro do bloco: `bar_in_block = (transport_bar % bars) + 1`.
- Repetição atual: `transport_bar / bars + 1` (limitada a `repeats` se `loop == false`).
- Ratio de stretch: `(sr * 60 / project_bpm) / (file_sr * 60 / file_bpm)` — simplificando,
  `project_bpm / file_bpm` ajustado pela SR. A guia acompanha o BPM da sessão, mantendo
  o pitch.
- Se `file_bpm` não for informado, estimar por autocorrelação de onset (worker) e deixar
  o usuário corrigir com um campo “BPM da guia”. **Nunca** inferir silenciosamente em
  material sem pulso.
- Start: a guia começa no bar 0 do transporte (ou após `count_in_bars`), no boundary.
- Stop da guia não para os loops; parar os loops não para a guia (são independentes,
  ambos ancorados na grade).

### 9.3 Monitoramento visual

- Timeline horizontal com a waveform da guia (peaks em mipmap), grade de compassos,
  marcadores de seção, playhead.
- Contador grande: `compasso 37 / 64 · repetição 1 / 2`.
- Barra de progresso do bloco e do total.
- Destaque dos últimos 2 compassos antes de uma mudança de seção/repetição.
- Para os loops: anel de progresso por canal + badge “termina em 1 compasso”.

---

## 10. Buses, roteamento e master

### 10.1 Grafo

```
canal 0..15 ──(gain, pan)──► bus A/B/C… (N envios) ──(gain)──► matriz ──► canais físicos
guia ───────────────────────► bus (padrão A)                    │
click/metrônomo ────────────► bus CUE (opcional)                 ▼
                                                             limiter/master
```

- Buses internos: estéreo (D6). Se o bus for mono, o pan do canal define o balanço.
- Matriz: cada bus pode ir para 1..N canais físicos, com ganho próprio (permite -inf).
- Master: ganho + limiter brickwall opcional (-0.3 dBFS) para proteger o PA.
- Solo/mute: resolvidos no control thread e enviados como ganho efetivo (evita lógica
  complexa no callback).

### 10.1.1 Envios múltiplos por canal

Um canal não escolhe "um bus": ele carrega uma **máscara de envios** (`bus_mask: u16`,
bit `b` = envia para o bus `b`). Isso permite mandar a mesma trilha para A **e** B ao
mesmo tempo — útil para somar um loop em dois destinos (ex.: A = PA, B = monitor, ou A+B
com processamentos diferentes na matriz).

Regras:

- o envio é unitário (sem ganho por envio, por enquanto); o ganho é o do canal;
- a máscara **nunca fica vazia**: desligar o último envio é ignorado;
- `SetBus(bus)` continua existindo como envio exclusivo (compatível com projetos e MIDI
  antigos); `SetBusSend { bus, on }` liga/desliga um destino;
- o render do canal monta o mix num scratch (`mix_l/mix_r`) e soma esse buffer em cada bus
  da máscara — o custo extra por bus é uma soma por amostra.

### 10.2 Mapa de saída

```rust
pub fn build_output_map(buses: &[Bus], device_channels: u16) -> OutputMap {
    // valida: device_channel < device_channels; buses sem output = mudos
    // pré-calcula, por canal físico, a lista de (bus_index, gain) que o alimenta
}
```

Pré-cálculo no control thread → o callback só percorre uma lista fixa.

---

## 11. Ableton Link

### 11.1 O que o Link oferece

- **Tempo** (BPM) compartilhado na sessão, com mudanças contínuas.
- **Fase de beat** compartilhada (permite alinhar o “1”).
- **Start/stop** compartilhado, opcional (`enableStartStopSync`).
- **Não** transmite áudio, nem posição de compasso, nem “qual seção”.

Consequência de projeto: a fase de compasso é **local** (derivada do nosso bar 0). O Link
alinha beat e tempo; o alinhamento de compasso é convenção nossa (e do peer, se ele também
for nosso app — aí podemos negociar via um “quantum” comum de 4 beats).

### 11.2 Política mestre/serva (o que o usuário pediu)

O Link não tem mestre: o tempo é um valor compartilhado que qualquer peer pode propor.
A política do app:

```rust
pub enum TempoAuthority {
    /// Usuário mexeu no BPM: publica no Link e ignora ecos por 300 ms.
    LocalCommit { until: Instant },
    /// Tempo veio da rede: segue, com suavização.
    Follow,
}
```

Regras:

1. Mudança local (UI/MIDI) → `set_tempo(bpm)` no Link + `TempoAuthority::LocalCommit`.
2. Mudança recebida → se `now > local_commit.until`, aplica (Follow). Se dentro da janela,
   é eco da própria mudança: ignora.
3. Mudanças contínuas (arrastar o fader) → publicar com throttle de 50 ms; o Link suaviza.
4. Link sem peers → comportamento local normal.
5. Link desconectado/erro → mantém o último tempo, marca `link.connected = false` na UI,
   tenta reconectar em background. Nunca para o áudio.
6. A mudança de tempo é aplicada **musicalmente**: agendada para o próximo boundary
   (compasso, por padrão) e suavizada no ratio de stretch; o BPM exibido muda na hora,
   o áudio acompanha em ~100–200 ms.

### 11.3 Integração

- Bindings para a lib C++ oficial (`ableton-link`), build via CMake; requer toolchain C++
  no Windows (MSVC) e LLVM/clang para bindgen.
- Thread dedicada com `Link::new(120.0)`, `enable_start_stop_sync(true)`,
  callback de `num_peers` para telemetria.
- O callback do Link **não** roda no áudio thread: publica em `rtrb`/atomic e o control
  thread aplica no tempo map.
- Teste sem rede: dois processos locais na mesma máquina (Link usa multicast em loopback)
  ou um peer de teste em Python/C++.

### 11.4 Riscos

- Firewall do Windows bloqueando multicast/UDP 20808 → documentar exceção.
- Wi-Fi com latência alta: Link tolera, mas a fase pode oscilar; usar `quantum` 4 e
  suavização evita “tremida” audível.
- Dois apps publicando BPM em loop (ping-pong): a janela de 300 ms + throttle resolve.

---

## 12. MIDI

### 12.1 Escopo

- Entrada por `midir` (todos os dispositivos, hot-plug a cada 2 s).
- Mapeamento de **CC**, **Note On/Off** e **Program Change** para parâmetros internos.
- `MidiLearn`: usuário clica em “learn” no parâmetro, move o controle, mapeamento criado.
- Modos de takeover: `Jump`, `Pickup` (recomendado), `Scale`.
- Feedback: LED de estado (Note On/Off) e valor (CC out) para controladores que suportam.
- Presets por controlador (ex.: “Launch Control XL”, “FCB1010”), salvos no projeto ou globais.

```rust
pub struct MidiMapping {
    pub source: MidiSource,        // CC { ch, cc } | Note { ch, note } | PC { ch }
    pub target: ParamTarget,       // ChannelGain(3), BusGain(A), TransportPlay, ...
    pub mode: TakeoverMode,
    pub min: f32, pub max: f32,    // curva
    pub invert: bool,
    pub feedback: Option<MidiFeedback>,
}
```

### 12.2 Parâmetros mapeáveis (v1)

Transporte (play/stop/tap), arm/record/play/stop por canal, gain por canal, gain por bus,
crop (início/comprimento, com snap), speed, direção, mute/solo, undo de camada, scroll da
timeline, marcadores da guia.

### 12.3 Regras

- MIDI nunca escreve direto no motor: vira `EngineCommand` no control thread, com o mesmo
  caminho de quantização da UI.
- CC de 7 bits → f32 com histerese (evita tremor em 1 LSB).
- `Pickup` implementado comparando o valor do controlador com o valor atual do parâmetro
  e só assumindo quando cruzar.
- MIDI clock **não** é usado como relógio (Link é o relógio); pode ser enviado como saída
  para gear externo em versão futura.

---

## 13. UI e telemetria

### 13.1 Layout: pads circulares

A UI é uma grade de **pads circulares** (4×2 por padrão, reflow para 3/2/1 colunas), não
uma lista de trilhas sobrepostas. O círculo é a metáfora correta porque o áudio é
recorrente: a posição no ciclo é um ângulo, e loops de tamanhos diferentes podem ser lidos
de relance como ponteiros de relógio analógico.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ LOOPSTATION [Tocar][Zero] ♩128.0 BPM quantum 4.0 Click  comp. 3·1.2      │
│                                        LINK off  [device ▾] [Abrir áudio] │
├──────────────────────────────────────────────┬───────────────────────────┤
│ ╭────────╮ ╭────────╮ ╭────────╮ ╭────────╮  │ CANAL 02  tocando         │
│ │ CH 01  │ │ CH 02  │ │ CH 03  │ │ CH 04  │  │ compasso 2 de 4           │
│ │ ◠ wave │ │ ◠ arc  │ │ ◠ rec  │ │ ◠ ...  │  │ material/crop ▓▓▓░░░      │
│ │  (⏸)   │ │  (⏸)   │ │  (■)   │ │  (▶)   │  │ período · material · ...  │
│ │ REC↶⤓✕│ │ REC↶⤓✕│ │ ...    │ │ ...    │  │ CROP tamanho −4+ início   │
│ │ ganho  │ │ ganho  │ │        │ │        │  │ VELOCIDADE 0.5 1 2 4      │
│ │ M S A B│ │ M S A B│ │        │ │        │  │ MIX ganho bus M S         │
│ ╰────────╯ ╰────────╯ ╰────────╯ ╰────────╯  │ [Overdub][Desfazer][Limpar]│
│ ╭────────╮ ╭────────╮ ╭────────╮ ╭────────╮  │                           │
│ │ CH 05  │ │ CH 06  │ │ CH 07  │ │ CH 08  │  │                           │
│ ╰────────╯ ╰────────╯ ╰────────╯ ╰────────╯  │                           │
├──────────────────────────────────────────────┴───────────────────────────┤
│ BUS A ▓▓▓░ M        BUS B ▓▓░░ M        dispositivo · entrada · status   │
└──────────────────────────────────────────────────────────────────────────┘
```

Cada pad tem altura fixa e largura fixa (alocação explícita, não `Grid`): foi o que
eliminou o vazamento de layout da primeira versão (slider de volume e barra de progresso
saindo da janela).

### 13.2 Anatomia de um pad

- **Anel externo**: ticks de beat (mais fortes na cabeça de compasso) e marcador de
  emenda às 12h.
- **Arco colorido**: fase do ciclo (`phase`), com cabeça branca. A cor vem do estado
  (verde tocando, vermelho gravando, laranja overdub, azul parado, amarelo armado).
- **Waveform em anel**: 72 pentes amostrados do envelope de 96 bins, mapeados pela janela
  de crop e pelo sentido — mostra **o que está tocando**, não o material inteiro.
- **Disco central**: glifo desenhado (play/pause/record/stop) e clique = ação primária
  (tocar/parar, ou armar/gravar em canal vazio).
- **Contagem regressiva de gravação**: com o canal armado, o disco central troca o glifo
  por um número grande (`4 → 3 → 2 → 1`) com o rótulo `beats`, e o arco externo **esvazia**
  proporcionalmente ao tempo restante (vermelho no último beat). O topo repete
  `● REC em N.N`. É o guia para atacar a nota na cabeça do tempo: o número vem do
  transporte (`rec_in_beats`), então não depende da latência que o músico ouve.
- **Linha de estado**: `período · velocidade`, contagem regressiva de beats quando armado,
  compassos já gravados durante a tomada, e "fim em X bar" no último compasso. Com tamanho
  definido, mostra `grava em 3.2 beats · 4 comp` (armado) e `gravando 2/4 comp` (tomada).
- **Ações**: REC (gravar/fechar/overdub), desfazer, abrir arquivo, limpar (2 cliques),
  sentido. Todas com glifos vetoriais — sem dependência de fonte de emoji. O tamanho da
  próxima tomada fica no painel lateral (`GRAVAR: livre · 1 · 2 · 4 · 8 · N comp`).
- **Mix no pad**: ganho, mute, solo, **envios A/B independentes** e velocidade (clicar
  cicla 0.5→1→2→4). Os botões A e B são toggles: o canal pode ir para os dois ao mesmo
  tempo (§10.1.1), e o rótulo mostra o destino atual (`A`, `B`, `A+B`).

O rodapé concentra o áudio do sistema:

- **entrada**: seletor de dispositivo + estado real (taxa, canais, `reamostrada`, erro);
- **VU da entrada** com *peak hold*, leitura em dBFS e um **ganho (trim)** de -24 a +24 dB,
  aplicado antes do motor — microfone interno de notebook costuma precisar de +12 a +24 dB;
- aviso "sinal muito baixo" quando há captura mas o pico fica abaixo de -55 dBFS, e dica
  traduzida quando o backend recusa a entrada (dispositivo em uso → sugerir ASIO).

O painel da direita repete os controles do canal selecionado com precisão (crop por
compasso, velocidade contínua, BPM de origem) e mostra a **faixa do material** com o crop
destacado e o cursor de reprodução — é ali que o músico vê "onde estou dentro da guia".

### 13.3 Regras de UI

- UI a 60 fps, mas desenho de waveform a partir de **envelopes pré-computados** (96 bins,
  recalculados só quando o material muda: `envelope_dirty`).
- Playhead interpolado entre snapshots (evita “tremida” de 60 Hz vs bloco de áudio).
- Toda interação gera comando; nenhum estado musical é mantido só na UI.
- Layout adaptativo: em janelas < 1280 px o seletor de dispositivo e o relógio de
  compasso saem do topo (continuam no painel/rodapé) em vez de colidir.
- Aviso de fim de loop: badge textual no último compasso + arco que muda de cor.
- Acessibilidade: espaço (play/stop), R (REC), U (undo), 1–8 (selecionar canal).

### 13.4 Snapshot e latência de UI

`triple_buffer` com `EngineSnapshot` (Copy, ~2 KB). UI lê o mais recente, sem esperar.
A UI pode ficar 1 frame atrás do áudio; isso é aceitável e explícito (não “consertar”
com locks).

---

## 14. Latência, calibração e entrada

- Latência de saída reportada pelo ASIO (`cpal::Stream::latency`) + latência de entrada.
- `round_trip_samples = output_latency + input_latency` (em amostras, na SR do motor).
- Compensação na gravação: o take é deslocado para trás por `round_trip_samples` (com
  clamp), de modo que o que o músico tocou no tempo X caia no beat X.
- Assistente de calibração: toca um clique, grava pelo loopback (cabo out→in), mede o
  atraso por correlação cruzada, salva em `calibration.json` por dispositivo.
- Buffer: 128 frames padrão (2.7 ms a 48 kHz), 256 como fallback; 64 se o driver aguentar.
- Deriva de clock entre dispositivos não é problema aqui (um só dispositivo); se houver
  entrada e saída em devices diferentes, medir e avisar.

### 14.1 Seleção da entrada (WASAPI)

No Windows, **entrada e saída são dispositivos distintos**, com nomes diferentes:

```
saída: "Alto-falantes (Saffire 6 USB 2.0)"
entrada: "Entrada (Saffire 6 USB 2.0)"
```

Comparar o nome inteiro nunca casa, então a escolha automática usa: nome idêntico → mesmo
grupo entre parênteses (identifica a placa) → palavras em comum
(`looper_ui::match_input_device`). O usuário pode sobrescrever pelo seletor **entrada** no
rodapé; trocar a entrada reabre o áudio.

### 14.2 Taxas diferentes e reamostragem

Placas fixam a entrada em outra taxa com frequência (ex.: Saffire 6 USB em 44,1 kHz com a
saída em 48 kHz). A versão inicial exigia taxa idêntica e **desabilitava a gravação em
silêncio** — sintoma: "entrada indisponível" sem explicação.

Agora:

1. `open_input` tenta configurações em ordem — taxa pedida, canais pedidos, taxa nativa,
   buffer fixo e depois `BufferSize::Default` — e devolve a configuração **realmente
   usada** no `InputSpec` do handle.
2. Se a taxa usada difere da do motor, o callback de saída reamostra com Catmull-Rom
   (`looper_audio::Resampler`), com buffer de pendências limitado (~170 ms) e silêncio em
   underrun.
3. O rodapé mostra o estado real: `entrada: … 2 ch @ 44100 Hz (reamostrada)` ou o erro do
   backend (ex.: `AUDCLNT_E_DEVICE_IN_USE`), em vez de um "indisponível" genérico. Cada
   **causa distinta** do backend aparece uma vez, com a taxa/canais da primeira tentativa
   que a produziu, e a UI traduz os códigos mais comuns (`looper_ui::audio_error_hint`).

Deriva de clock entre entrada e saída (cristais diferentes) é absorvida pelo buffer; se
persistir por minutos, aparece como underrun/overflow ocasional. Correção adaptativa de
deriva fica para o M6.

### 14.2.1 Alinhamento da janela de entrada (crítico)

A janela de captura usada na tomada é indexada pelo **bloco recém-escrito no anel de
entrada**, não pelo sample do transporte:

```
base = input_block_start + offset - input_delay_frames
```

Usar `transport.sample()` funcionava só enquanto o transporte rodava desde o início da
sessão. Ao parar o transporte (ou dar Reset), o sample congela enquanto o anel continua
sendo alimentado — a diferença cresce em tempo real e, acima da capacidade do anel
(~1,4 s a 48 kHz), a janela cai fora dele e vira **silêncio**. Sintoma: VU de entrada com
sinal, tomada gravada e loop mudo.

Teste de regressão: `recording_after_a_long_pause_still_captures_input` (para por 4 s,
retoma, grava 1 compasso). Com a indexação antiga o take sai com pico 0.

### 14.3 Interfaces que só aceitam um stream (caso real)

Medido nesta máquina com o **Saffire 6 USB 2.0**: a entrada abre sozinha em 44,1 kHz, mas
falha com `AUDCLNT_E_DEVICE_IN_USE` enquanto a saída da mesma placa está aberta — e o
inverso também falha (saída bloqueada enquanto a entrada está aberta). É limitação do
driver WDM dessa interface: **um stream por vez**, mesmo em modo compartilhado.

Consequências práticas:

- com WASAPI, essa placa só reproduz *ou* só grava; para gravar tocando é preciso **ASIO**
  (um único stream duplex), que exige compilar com a feature `asio` e o SDK da Steinberg;
- a UI detecta o caso (erro "device in use" na entrada com a saída aberta) e sugere ASIO;
- outras placas (aqui, o codec Realtek) abrem entrada e saída ao mesmo tempo sem problema —
  inclusive em taxas diferentes, via reamostragem.

Diagnóstico rápido sem abrir a GUI:

```powershell
cargo run -p looper-app --bin looper-cli -- inputs          # lista entradas e formatos
cargo run -p looper-app --bin looper-cli -- probe-input      # abre a entrada e conta amostras
cargo run -p looper-app --bin looper -- --diag               # toca, grava, fecha e confere o loop
cargo run -p looper-app --bin looper -- --diag "" 0 18       # idem com +18 dB de trim na entrada
```

O `--diag` abre saída e entrada, toca, grava, **fecha a tomada quantizada** e imprime:
estado da entrada, pico do sinal (pós-trim), pico da tomada, estado final do canal, número
de camadas e o material consolidado. É o teste de fumaça do caminho de captura inteiro.

### 14.4 VU de entrada e ganho (trim)

Microfone interno de notebook entrega tipicamente **-50 a -70 dBFS**: o take é gravado, mas
soa como silêncio. Sem medidor, não há como distinguir "microfone mudo" de "microfone
baixo" — foi exatamente o relato do primeiro teste na placa interna.

Por isso o rodapé tem:

- VU do sinal que chega ao motor (pós-trim), com queda suave, *peak hold* de 1,2 s e leitura
  em dBFS;
- **ganho de entrada** de -24 a +24 dB, aplicado no callback de saída antes de entregar ao
  motor (`InputMeter` com `AtomicU32`, sem lock);
- aviso "sinal muito baixo" quando há captura mas o pico fica abaixo de -55 dBFS.

O mesmo `InputMeter` guarda o pico máximo desde a abertura, usado pelo `--diag`.

### 14.5 Compensação de latência de entrada (corte do início)

A compensação **não desloca a leitura da entrada** — ela **corta o começo da tomada** e
grava `comp` a mais no fim. É o modelo correto (e o inverso do que era feito antes):

```
comp = L frames (latência de ida e volta do setup)

início:  rec_start = beat da cabeça
         agenda FinishRecord em (fim musical) + L beats
         reserva N compassos + L frames
tomada:  o áudio é escrito na posição natural (transport - rec_start)
fim:     descarta os primeiros L frames  ->  sobra exatamente o material,
         com o ataque no beat 1 e o último tempo preservado
overdub: o buffer é somado **rotacionado** por -L dentro do loop
```

Deslocar a janela para trás (implementação antiga) **atrasava** o ataque: com `comp`
maior, a nota aparecia mais tarde no loop — efeito contrário ao esperado. O teste
`comp_trims_the_input_latency_from_the_take_head` cobre o caso: um ataque que chega `L`
frames depois do beat cai na amostra 0 com `comp = L`, e fica atrasado com `comp = 0`.

As três parcelas que formam `comp` continuam iguais: anel de entrada medido
automaticamente (rodapé mostra `buffer N ms`), `comp` manual e calibração acústica
(botão `calibrar`, ~2 s de clique com captura de entrada e saída no mesmo bloco).

> **Sintoma × causa**: "gravei 1-2-3-4, 1-2-3 e o último 4 foi cortado" = **um beat de
> atraso de entrada** (~500 ms a 120 BPM). Com `comp` em 0 a tomada começa atrasada e
> termina antes do último tempo; o corte do início devolve o alinhamento. Aumentar `comp`
> agora **adianta** o ataque no loop (antes, atrasava).

---

## 15. Persistência

```
MySong.looper/
├── project.json          # modelo completo (versão, canais, buses, guia, MIDI, tempo map)
├── audio/
│   ├── guide.wav         # cópia do arquivo importado (float32, SR do motor)
│   ├── ch1_base.wav
│   ├── ch1_od1.wav
│   └── ...
├── peaks/
│   └── ch1_base.pk       # mipmap de min/max
├── calibration.json
└── undo/                 # camadas removidas por commit (recuperação)
```

- `project.json` tem `schema_version`; migrações explícitas por versão.
- Áudio sempre em WAV float32 na SR do motor (conversão na importação, uma vez).
- Caminhos relativos à pasta do projeto → mover a pasta não quebra nada.
- Autosave a cada 30 s e em toda mudança estrutural (não em cada fader).
- Undo global: pilha de comandos invertíveis no control thread (limite 200), separado do
  undo de camadas por canal.
- Se o app crashar gravando: WAVs são escritos incrementalmente com header corrigido no
  fechamento; no pior caso, recuperação por varredura de tamanho.

---

## 16. Estrutura de crates

```
looper/
├── Cargo.toml                # workspace
├── crates/
│   ├── looper-core/          # timeline, tempo map, modelo, quantização, crop, sem I/O
│   ├── looper-stretch/       # trait Stretcher + backends (signalsmith, rubberband, passthrough)
│   ├── looper-decode/        # symphonia, resample, peaks, BPM estimate
│   ├── looper-engine/        # grafo RT, canais, buses, gravação, telemetria
│   ├── looper-audio/         # cpal + ASIO, enumeração, calibração
│   ├── looper-link/          # wrapper seguro do Ableton Link
│   ├── looper-midi/          # midir, learn, mapping, presets
│   ├── looper-project/       # serde, WAV io, migrações, autosave
│   ├── looper-ui/            # egui: timeline, widgets, temas
│   └── looper-app/           # binário: wiring, CLI, logging
├── docs/
│   ├── architecture.md       # este documento
│   ├── adr/                  # decisões pontuais
│   └── manual/               # guia do usuário (fase M5)
└── tests/
    ├── golden/               # renders de referência
    └── harness/              # render offline determinístico
```

### 16.1 Direção de dependências

```
core ◄── stretch
  ▲        ▲
  │        │
decode ◄── engine ◄── audio
  ▲          ▲
  │          │
project ─────┤
             │
link ────────┤
midi ────────┤
             │
ui ──────────┘  (ui depende só de core + tipos de telemetria)
app  ──► todos
```

Regra: `core` não conhece áudio, disco, rede nem UI. `engine` não conhece UI nem `cpal`.
`ui` não conhece `engine` (só os tipos de snapshot/comando, que vivem em `core`).

### 16.2 Dependências principais

| Crate | Uso | Observação |
|---|---|---|
| `cpal` (feature `asio`) | I/O de áudio | requer ASIO SDK da Steinberg (não redistribuível) e LLVM/clang |
| `symphonia` | decode WAV/MP3/FLAC/OGG | offline only |
| `rubato` | resample de SR | offline only |
| `signalsmith-stretch` / `rubberband` | time-stretch | ver §7.3 |
| `ableton-link` | Link | compila lib C++ via CMake |
| `midir` | MIDI | Windows MIDI API |
| `egui`/`eframe` | UI | |
| `rtrb`, `triple_buffer`, `arc-swap` | comunicação RT-safe | |
| `serde`, `serde_json` | projeto | |
| `hound` | WAV I/O | ou `symphonia` para leitura |
| `anyhow`, `thiserror` | erros | |
| `tracing` | logs (fora do callback) | |

---

## 17. Regras de tempo real (checklist de revisão)

- [ ] Nenhuma alocação no callback (`Vec`, `Box`, `String`, `format!`).
- [ ] Nenhum `Mutex`/`RwLock`/`Condvar` no callback.
- [ ] Nenhum I/O (arquivo, rede, log, stdout) no callback.
- [ ] Nenhum `panic!`/`unwrap`/indexação sem checagem em caminho de áudio.
- [ ] Denormais desligados (FTZ/DAZ) no thread de áudio.
- [ ] Buffers pré-alocados para o pior caso de bloco (ex.: 2048 frames).
- [ ] Comandos aplicados em offsets de sample, não no início do bloco por preguiça.
- [ ] Fades em toda transição de ganho/estado (evita clique).
- [ ] Telemetria publicada no máximo a cada ~16 ms (não a cada bloco).
- [ ] Nenhum `Arc::clone` no callback (clone só no control thread, via swap).
- [ ] Testes com `--release` e com bloco 64 para achar overrun.

---

## 18. Testes e harness offline

### 18.1 Harness determinístico

O motor é uma função pura do tempo quando a I/O é substituída por um device falso:

```rust
let mut engine = Engine::new(EngineConfig {
    sample_rate: 48_000,
    block_size: 128,
    channels: 8,
    ..Default::default()
});

engine.submit(at_beat(0.0), EngineCommand::Transport(TransportCmd::Play));
engine.submit(at_beat(0.0), EngineCommand::Channel { id: ChannelId(0), cmd: ChannelCmd::Arm });
// ... roteiro
let out = engine.render_offline(Duration::from_secs(30)); // Vec<f32> estéreo
```

- Saídas comparadas por hash + tolerância de RMS/peak (não por igualdade bit a bit, por
  causa de SIMD/float).
- Testes de propriedade para: `period_beats`, fase, crop, quantização, tempo map inverso.

### 18.2 Casos de teste obrigatórios

| Teste | Critério |
|---|---|
| Loop de 4 compassos, 120 BPM, 30 s | seam sem descontinuidade > -60 dBFS de salto |
| Loop de 8 compassos cropado para 2 | período = 2 compassos; material preservado |
| Dois loops de 2 e 8 compassos | realinham no compasso 8 |
| BPM 120 → 128 em play | pitch inalterado (±5 cents), fase mantida no boundary |
| Speed 2× | período metade, pitch preservado |
| Reverse em play | troca no boundary, sem estalo |
| Overdub + undo | mix idêntico ao estado anterior (tolerância 1e-6) |
| Gravação com latência simulada | take alinhado ao beat (erro < 1 ms) |
| Bus A→L, B→R | soma correta por canal físico |
| Tempo map com 100 mudanças | sem drift (beat_at(sample_at(b)) ≈ b) |

### 18.3 Testes de integração

- ASIO real: 30 min de execução, 0 xruns, CPU < 60%.
- Link: dois processos, mudanças de BPM de 20 a 300, fase estável.
- MIDI: controlador real, learn + pickup + feedback.
- Projeto: salvar, fechar, abrir, comparar render.

---

## 19. Orçamento de memória

| Item | Cálculo | Tamanho |
|---|---|---|
| Loop 8 compassos, 120 BPM, estéreo 48k f32 | 16 s × 48k × 2 × 4 B | ~6,1 MB |
| 8 canais × 4 camadas | 8 × 4 × 6,1 MB | ~195 MB |
| Guia 192 compassos, 120 BPM | 384 s × 48k × 2 × 4 B | ~147 MB |
| Guia 64 compassos × 2 repetições | 128 s (arquivo único) | ~49 MB |
| Cache de stretch (1 ciclo por canal) | ~6 MB × 8 | ~49 MB |
| Peaks (mipmap) | ~1% do áudio | ~4 MB |
| **Total típico** | | **~450 MB** |

Limites configuráveis: máximo de camadas, máximo de minutos por canal, aviso em 80% do
orçamento. Se o usuário importar uma guia de 1 hora, oferecer streaming do disco (fase 2).

---

## 20. Riscos e mitigação

### 20.1 Licenciamento (bloqueante para “open source no GitHub”)

- **Ableton Link**: GPLv2+ **ou** licença comercial. Se o app usa a lib oficial, o binário
  distribuído precisa ser GPL-compatível. Decisão: **GPL-3.0-or-later** para o projeto
  inteiro (compatível com GPLv2+). Se no futuro quiser fechar o app, é preciso licença
  comercial do Link — orçar antes.
- **ASIO SDK**: não redistribuível. O build do usuário precisa baixar o SDK da Steinberg
  e apontar `ASIO_SDK_DIR`. Não commitar o SDK. Documentar no README e no CI.
- **Rubber Band**: GPLv2/comercial — ok sob GPL-3.0-or-later. Signalsmith (MIT) evita
  o problema se algum dia a licença mudar.

### 20.2 Técnicos

| Risco | Impacto | Mitigação |
|---|---|---|
| Qualidade do stretch em ratios extremos | artefatos audíveis | limitar a 0.5–2×; cache offline; backend trocável |
| CPU estourada com 8 canais + guia | xruns | orçamento medido no M1; degradação graciosa (desligar RT quando estável) |
| Desalinhamento de fase no seam | loop “escorrega” | stretch dirigido pela grade (§7.3) + crossfade |
| Driver ASIO instável | áudio para | thread de watchdog, fallback WASAPI, mensagem clara |
| MP3 com delay de encoder | guia deslocada | preferir WAV/FLAC; correção gapless |
| Link com Wi-Fi ruim | BPM oscilando | suavização + quantum 4 + histerese |
| Gravação durante mudança de BPM | take inconsistente | D13: travar tempo durante a tomada |
| Undo de overdub consumindo RAM | OOM | limite de camadas + commit para disco |
| UI a 60 fps pesada com 8 waveforms | UI travando | peaks em mipmap + desenho por nível de zoom |

---

## 21. Roadmap

### M0 — Fundação (1–2 semanas) — ✅ concluído

- Workspace Cargo com os 10 crates, CI local (`cargo test`, `clippy`, `fmt`).
- `looper-core`: tempo map, quantização, crop, testes de propriedade.
- `looper-engine`: transporte + metrônomo + grafo vazio; harness offline.
- `looper-audio`: enumeração de devices, abrir ASIO, callback com silêncio.
- **Aceite:** click a 120 BPM audível, 0 xruns em 10 min, `render_offline` determinístico.

> Entregue também: decode (WAV/MP3/FLAC/OGG), política do Link, MIDI learn, schema de
> projeto em JSON + WAV float32, view-model da UI e a CLI `looper-cli`. A GUI egui abre o
> dispositivo padrão e conversa com o motor por filas lock-free.

### M1 — 8 canais gravando — 🟡 núcleo entregue

- ✅ Gravação quantizada (arm → boundary → take), overdub em camadas (até 8) e undo.
- ✅ Play/stop por canal, waveform em anel + playhead.
- ⬜ Calibração automática de latência de ida-e-volta (hoje `input_delay_frames` fixo).
- **Aceite:** gravar 4 compassos no ch1, ouvir em loop sem estalo, alinhado ao click.

### M2 — Trilha guia (1–2 semanas) — ⬜ pendente

- Import WAV/MP3/FLAC, decode offline, peaks, BPM da guia, blocos e repetições.
- Timeline da guia com contador de compassos e seções.
- **Aceite:** guia de 64 compassos × 2 com loops por cima, contador correto, aviso de fim.

### M3 — Crop, direção, speed, buses — ✅ concluído

- ✅ Crop não destrutivo (material intacto; só muda o período executado).
- ✅ Reverse, velocidade 0.25×–4×, ganho por canal, pan, mute/solo.
- ✅ Buses A/B com ganho e mute; matriz de saída na engine.
- **Aceite:** cenário do usuário (ch1–2→A→L, ch3–8→B→R) e crop 8→2.

### M4 — Time-stretch — 🟡 RT entregue

- ✅ WSOLA dirigido pela grade em tempo real, pitch preservado (testes de 220 Hz a 2×/0,5×).
- ✅ Leitura periódica do crop (seam contínuo) e reverse espelhado.
- ⬜ Re-render offline de qualidade máxima quando o BPM stabiliza (§7.2).
- **Aceite:** BPM 120→128 em play, pitch ±5 cents, fase mantida, sem estalo.

### M5 — MIDI + Link (2 semanas)

- Learn, pickup, feedback, presets; sessão Link, política mestre/serva, start/stop.
- **Aceite:** dois peers sincronizados; controlador mapeado sem tocar no mouse.

### M6 — Projeto, robustez, release (2 semanas)

- Salvar/abrir, autosave, recuperação de crash, migrações, manual, CI de release.
- **Aceite:** ciclo completo de ensaio salvo e reaberto idêntico.

> A ordem coloca o stretch depois do crop porque o crop define `period_beats`, que é a
> entrada do stretch. Se o usuário preferir ouvir BPM variável antes, M4 pode subir para
> depois do M1 usando varispeed como provisório — mas isso muda o aceite do M1.

---

## 22. Questões abertas (não bloqueiam o M0)

1. ~~**Speed 2×**: confirmar a semântica de período (metade dos compassos, pitch preservado)
   vs. varispeed clássico.~~ **Resolvido:** período encolhe e o pitch é preservado
   (`period_beats = crop.length_beats / speed`); o modo fita fica para depois.
   Ver [`docs/adr/0001-speed-2x-semantics.md`](adr/0001-speed-2x-semantics.md).
2. **Crop por beat ou só por compasso?** O documento permite beat com snap em compasso.
3. **Guia com múltiplas seções e repetições por seção** (ex.: A×4, B×2) ou só bloco × N?
   v1: bloco × N + marcadores visuais.
4. **Quantização padrão do Record**: compasso (assumido) ou beat?
5. **Solo/mute**: solo exclusivo ou aditivo? v1: aditivo.
6. **Metrônomo em bus separado** (cue) ou junto? v1: bus próprio, opcional.
7. **Limite de camadas de overdub** e política de commit automático.

### 22.1 Resolvidos na fundação (M0)

- **Semântica do speed 2×** — ADR
  [`0001-speed-2x-semantics.md`](adr/0001-speed-2x-semantics.md).
- **Costura do loop no M0** — o motor usa *seam fade* equal-power (fade-in/fade-out de
  5 ms) em vez do crossfade sobreposto de §8.6; o upgrade para crossfade cauda↔cabeça
  fica para o M4. ADR [`0002-seam-fade.md`](adr/0002-seam-fade.md).
- **Interface em pads circulares** — a lista de trilhas foi substituída por pads
  circulares com altura fixa; ADR [`0003-pads-circulares.md`](adr/0003-pads-circulares.md).
- **Time-stretch em tempo real** — WSOLA em Rust, dirigido pela grade, com leitura
  periódica do crop; ADR [`0004-wsola-tempo-real.md`](adr/0004-wsola-tempo-real.md).

---

## 23. Glossário

| Termo | Significado |
|---|---|
| **Transporte** | relógio musical único do app; fonte de verdade de tempo e posição |
| **Quantum** | grade mínima para disparo/gravação (1 beat, 1 compasso, …) |
| **Boundary** | instante musical em que uma mudança é aplicada (início de compasso/beat) |
| **Take** | uma gravação bruta; vira camada de um canal |
| **Camada** | take imutável somado ao mix do canal (overdub) |
| **Crop** | janela não destrutiva que define o trecho audível e o novo período |
| **Período** | duração musical efetiva do ciclo do canal (`crop / speed`) |
| **Seam** | costura entre o fim e o início do loop |
| **Bus** | soma intermediária estéreo, com ganho próprio, roteada para a placa |
| **Bloco da guia** | unidade da guia (`bars`) repetida `repeats` vezes |
| **Ratio** | amostras de saída / amostras de entrada no stretcher |
| **xrun** | falha de deadline do buffer de áudio |

---

## 24. Referências de implementação

- Ableton Link (protocolo e API C++): <https://github.com/Ableton/link>
- `cpal` (ASIO/WASAPI): <https://github.com/RustAudio/cpal>
- `symphonia` (decode): <https://github.com/pdeljanov/Symphonia>
- Signalsmith Stretch: <https://github.com/Signalsmith-Audio/signalsmith-stretch>
- Rubber Band: <https://breakfastquay.com/rubberband/>
- `egui`: <https://github.com/emilk/egui>
- `midir`: <https://github.com/Boddlnagg/midir>

---

*Este documento é a fonte de verdade da arquitetura. Mudanças de decisão devem virar um
ADR em `docs/adr/` e atualizar a tabela da §2.*
