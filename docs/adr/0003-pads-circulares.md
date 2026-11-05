# ADR 0003 — Pads circulares como unidade de interface

- **Status:** aceito
- **Data:** 2026-11-01
- **Relacionado:** §13 do `docs/architecture.md`, D9

## Contexto

A primeira versão da GUI listava os canais como trilhas empilhadas (padrão de editor
multifaixa, tipo Audacity): cabeçalho, barra de progresso linear, sliders e botões numa
linha. Três problemas ficaram evidentes no teste com áudio real:

1. **Leitura ruim em performance.** Com 8 loops de tamanhos diferentes, barras lineares
   não comunicam "onde cada loop está" sem comparar comprimentos de barras diferentes.
2. **Vazamento de layout.** `ProgressBar` dentro de um `horizontal` consome a largura
   disponível e empurra o slider de volume para fora da janela; o mesmo aconteceu com o
   playhead linear. Era um bug de layout, mas também sintoma do modelo errado.
3. **Gravação invisível.** O fluxo principal de uma loopstation — gravar uma tomada e
   fechar o loop — não tinha botão no canal.

Loopstations digitais (as referências enviadas: Electro-Harmonix 95000, Pigtronix
Infinity, Enso) usam **círculos** justamente porque o áudio é recorrente: a posição no
ciclo é um ângulo. Loops de 2 e de 8 compassos giram em velocidades diferentes e o
músico lê a fase de todos de relance, como ponteiros de relógio.

## Decisão

Cada canal é um **pad circular** com altura e largura fixas, dentro de uma grade que
reflui (4×2 padrão; 3/2/1 colunas em janelas menores). O círculo contém:

- arco externo = fase do ciclo (com cabeça de playhead);
- ticks de beat/compasso + marcador de emenda às 12h;
- waveform em anel = envelope do **trecho em execução** (respeita crop e sentido);
- disco central clicável = play/parar (ou armar/gravar em canal vazio);
- abaixo do círculo: REC, desfazer, abrir, limpar (2 cliques), sentido, ganho, M/S, bus A/B
  e velocidade.

O detalhe preciso do canal selecionado fica num painel lateral, incluindo a faixa do
material com o crop destacado e o cursor — necessário porque o anel mostra o ciclo, não a
posição dentro do arquivo.

## Consequências

- Posicionamento explícito (`UiBuilder::max_rect` + largura fixa) em vez de `Grid`; foi o
  que eliminou o vazamento. `Frame` herda o layout do pai, então o conteúdo do pad é
  forçado a `vertical`.
- Glifos são desenhados com formas (triângulo, barras, círculo, quadrado) em vez de
  caracteres — não dependem da fonte de emoji e ficam nítidos em qualquer DPI.
- A UI ganhou um **renderizador headless de teste** (`src/preview.rs`) que rasteriza a
  mesma árvore de shapes em PNG, permitindo revisar layout sem abrir janela.
- Custo: menos informação por pad que uma trilha; o painel lateral compensa.
