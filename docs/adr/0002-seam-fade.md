# ADR 0002 — Fade de costura equal-power no M0

- **Status:** aceito (solução provisória; substituída por crossfade sobreposto no M4)
- **Data:** 2026-11-01
- **Relacionado:** D11, §8.6 do `docs/architecture.md`

## Contexto

Todo loop tem uma costura (*seam*): o instante em que o fim do ciclo volta para o
começo. Se o material não for contínuo nesse ponto — e normalmente não é —, a
descontinuidade produz um estalo audível. O mesmo vale para crop, troca de direção,
troca de speed e play/stop.

A arquitetura pede um crossfade de costura de 5 ms (2–20 ms). O crossfade clássico
sobrepõe a **cauda** do ciclo anterior (lida além do fim do trecho) com a **cabeça** do
ciclo novo, o que preserva o nível e a energia. Ele exige, porém, ler material fora da
janela do crop e misturar duas posições de leitura simultâneas — algo que só faz sentido
quando o backend de time-stretch (M4) já mantém estado de leitura próprio.

No M0 o motor ainda usa `LinearStretcher` e lê o material por interpolação direta. Para
não adiar o aceite do milestone, precisamos de uma solução que garanta continuidade sem
introduzir um segundo leitor.

## Decisão

No M0 aplicamos um **seam fade equal-power** (fade-in/fade-out), não um crossfade
sobreposto:

```
fade_frac = min(crossfade_ms_em_amostras / period_samples, 0.5)

p < fade_frac        → sin((p / fade_frac) * π/2)
p > 1 - fade_frac    → sin(((1 - p) / fade_frac) * π/2)
caso contrário       → 1.0
```

- A amplitude é exatamente **zero no seam**, e sobe/desce em curva equal-power
  (seno), o que soa mais natural que uma rampa linear.
- A janela padrão é `DEFAULT_CROSSFADE_MS = 5.0` ms, configurável por canal via
  `ChannelCmd::SetCrossfadeMs` (2–20 ms na UI).
- `fade_frac` é limitado a 50% do período, então loops muito curtos não são engolidos
  pelo fade.
- O fade **não desloca fase nem altera o período**: a geometria da grade continua
  idêntica à do ADR 0001.
- O portão de play/stop (`gate`) é independente e usa rampa linear curta para o
  declick de transporte.

## Consequências

- **Positivas:** elimina o estalo no seam sem segundo leitor, sem estado extra e sem
  alocação no callback; determinístico e trivial de testar offline; funciona em
  qualquer speed/direção.
- **Negativas:** há um entalhe de amplitude de ~5 ms por ciclo. Em material percussivo
  isso pode comer o transiente do primeiro golpe. Por isso a janela é configurável por
  canal e o valor padrão é conservador.
- **Upgrade previsto (M4):** substituir por crossfade sobreposto cauda↔cabeça dentro do
  backend de stretch, mantendo a mesma configuração `crossfade_ms`. O motor não muda de
  interface; muda apenas a implementação do trecho de render.
- **Testes:** o harness offline verifica que a borda do ciclo tende a zero e que não há
  descontinuidade acima do limiar audível.

## Alternativas descartadas

| Alternativa | Motivo |
|---|---|
| Crossfade sobreposto já no M0 | exige dois leitores e material além do crop; pertence ao M4 |
| Rampa linear de 1 ms | ainda deixa degrau audível em material tonal |
| Sem fade, confiando no material | o usuário grava material arbitrário; estalo garantido |
| Fade fixo global | material percussivo e pads pedem janelas diferentes; por isso é por canal |
