# ADR 0001 — Semântica do speed 2×

- **Status:** aceito
- **Data:** 2026-11-01
- **Decisores:** usuário + implementação
- **Relacionado:** D2, D4, D11, §8.4, §8.5 do `docs/architecture.md`

## Contexto

O canal precisa oferecer velocidade. Há duas famílias de comportamento possíveis:

1. **Período encolhe, pitch preservado** — o loop continua ancorado na grade; 8
   compassos a 2× ocupam 4 compassos e o material soa igual, só mais rápido.
2. **Varispeed (modo fita)** — o material é lido mais rápido, o pitch sobe e a duração
   musical não muda.

O requisito declarado pelo usuário é que mudanças de BPM (inclusive vindas do Ableton
Link) **não alterem o pitch**. Como o BPM é a variável mestra e os loops são ancorados
na grade global, a velocidade do canal precisa seguir a mesma lógica: mudar a taxa de
eventos musicais, não a altura.

## Decisão

Adotamos a opção 1:

```
period_beats = crop.length_beats / speed
phase(beat)  = ((beat - anchor_beat) mod period_beats) / period_beats
input_beat   = crop.start_beat + phase * crop.length_beats      (forward)
             = crop.start_beat + (1 - phase) * crop.length_beats (reverse)
```

- `speed` é um multiplicador de **taxa musical** (`0.5×`, `1×`, `2×`, …), normalizado em
  `looper-core::normalize_speed`.
- O período é derivado, nunca acumulado: a fase é sempre recalculada a partir do beat do
  transporte, então não há drift ao longo de horas.
- Crop e speed **compõem**: o crop define o trecho e o speed define quão rápido ele é
  percorrido. A UI restringe combinações para que `period_beats` seja múltiplo de 1 beat
  (idealmente de 1 compasso).
- O material original nunca é reescrito; mudar `speed` de volta para `1×` restaura o
  comportamento anterior.

O modo fita (varispeed) fica como opção futura por canal, explicitamente **fora** do
aceite do M3.

## Consequências

- **Positivas:** loops de tamanhos diferentes convivem na mesma grade; crop e speed são
  ortogonais; o cálculo é puro e testável offline.
- **Negativas:** exige time-stretch com preservação de pitch no caminho de áudio. Sem
  isso, a leitura linear do material acelera junto e o pitch sobe.
- **Estado da implementação:** no M0 o motor usa `LinearStretcher`
  (`looper-stretch`), que lê o material 1:1 e portanto **é varispeed**. A geometria da
  grade já é a definitiva; o backend com pitch preservado (WSOLA/Rubber Band) entra no
  M4 sem mudar `looper-core` nem a interface do motor.
- **Testes:** `looper-core::loop_channel` cobre `speed_halves_the_period_without_touching_crop`
  e `crop_and_speed_compose`; o harness offline do motor valida a fase e o seam.

## Alternativas descartadas

| Alternativa | Motivo da rejeição |
|---|---|
| Varispeed como comportamento padrão | muda o pitch ao seguir o Link, contrariando o requisito |
| Speed alterando o comprimento do buffer | destrutivo, quebra o crop não destrutivo e o undo |
| Speed aplicado no bus/master | afetaria todos os canais e a guia; a decisão é por canal |
