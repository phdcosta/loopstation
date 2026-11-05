# ADR 0004 — WSOLA em tempo real dirigido pela grade

- **Status:** aceito
- **Data:** 2026-11-01
- **Relacionado:** §7 do `docs/architecture.md`, D2/D3, ADR 0001

## Contexto

O motor precisava tocar loops com **BPM variável** (usuário ou Ableton Link) **sem mudar o
pitch**. A primeira versão apenas mudava a taxa de leitura (`sample_interp`), ou seja,
varispeed: ao alterar a velocidade o loop transposicionava junto, violando o requisito.

A arquitetura já previa dois backends possíveis (WSOLA/phase-vocoder em RT + re-render
offline) e um contrato "dirigido pela grade": o motor informa a posição de entrada
derivada do transporte, e o algoritmo só escolhe grãos próximos dela. Isso evita a deriva
clássica de stretchers livres.

Restrições adicionais:

- Sem dependência C++ nova (o ASIO já é uma dor de build no Windows).
- Determinístico, para os testes offline do motor valerem.
- Estéreo, com crop, reverse e seam contínuo.

## Decisão

Implementar **WSOLA** puro em Rust (`looper-stretch::WsolaStretcher`):

- Frame 1024, hop 512, busca hierárquica (passo 8 + refinamento unitário) de ±384 amostras
  a 48 kHz, maximizando correlação normalizada com a continuação natural do grão anterior.
- Sobreposição com metades complementares de Hann (`w_head + w_tail = 1`).
- A posição nominal de cada grão é recalculada **a partir da grade** a cada bloco
  (`start + n·ratio`), não acumulada — é o que mantém a fase presa ao transporte.
- `LoopWindow` faz leitura periódica com interpolação dentro do crop: o seam é contínuo
  por construção, e o reverse é um espelho da leitura (`at(u, reverse=true)`).
- Em `ratio ≈ 1` o motor faz leitura direta periódica (bit-exata); o WSOLA só entra quando
  há stretch.

## Consequências

- Pitch preservado: senoide de 220 Hz a 2× continua em ~220 Hz (modo fita daria 440 Hz);
  o mesmo a 0,5×. Coberto por teste no crate e no motor.
- O estado é por hop, não por bloco: renderizar em blocos de 128 ou 256 dá o mesmo sinal,
  requisito porque o motor fatia blocos nos comandos agendados.
- Qualidade: boa para material rítmico; em material muito tonal há leve "smear". O
  re-render offline (Signalsmith/Rubber Band) continua planejado para quando o BPM
  estabiliza (§7.2), com troca no boundary.
- Não implementado ainda: compressão/expansão automática do último trecho quando o ratio
  muda no meio do ciclo (§7.5); hoje o loop mantém a fase e o WSOLA reacomoda os grãos.
