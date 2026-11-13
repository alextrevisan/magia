# magia — Plano de implementação

Harness de agente de código **100% Rust**, com GUI nativa em **egui/eframe + fastframe**, conectando a:

| Provedor | Base URL | Formato(s) | Auth |
|---|---|---|---|
| OpenCode Zen (pay-as-you-go) | `https://opencode.ai/zen/v1` | Chat Completions; Responses (modelos GPT); Messages (modelos Claude, base `https://opencode.ai/zen`) | `Authorization: Bearer $OPENCODE_API_KEY` (`x-api-key` no Messages) |
| OpenCode Go (assinatura) | `https://opencode.ai/zen/go/v1` | Chat Completions; Messages para alguns modelos (MiniMax, Qwen…) em `https://opencode.ai/zen/go` | mesma chave `OPENCODE_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | Chat Completions (OpenAI-compatível) | `Authorization: Bearer $OPENROUTER_API_KEY` |

O catálogo de modelos é sempre buscado em `GET {base}/models`, nunca hard-coded.

---

## 0. Requisitos não funcionais (prioridade máxima)

Estes objetivos têm precedência sobre features: uma feature que os viole fica atrás de flag ou não entra.

### 0.1 CPUs antigas (sem AVX/AVX2/SSE4)

- **Alvo de compilação: x86-64 baseline (v1, só SSE2).** É o padrão do `x86_64-unknown-linux-gnu`; **proibido** `-C target-cpu=native`, `x86-64-v2/v3` ou `target-feature=+avx*` em `.cargo/config.toml` ou nos scripts de release.
- Dependências com SIMD só se tiverem **detecção em tempo de execução** com fallback escalar (ex.: `memchr` e `ring` têm). Nada de crates que exigem AVX em tempo de compilação (ex.: `simd-json` sem fallback).
- TLS: `rustls` com provider **`ring`** (detecção de CPU em runtime, compila sem cmake), não o `aws-lc-rs` padrão.
- **Renderização: sem Vulkan, sem `wgpu`. eframe só com backend `glow` (OpenGL).** Máquinas com CPU antiga costumam ter GPU antiga sem Vulkan; `glow` roda em OpenGL 2.1/ES 2.0 e no Mesa `llvmpipe` (software, sem GPU nenhuma).
  - O `default` do eframe 0.36.2 **inclui `wgpu`**, então ele é declarado com `default-features = false, features = ["glow", "wayland", "x11", "default_fonts"]` (o mesmo que o workspace do fastframe usa).
  - `fastframe-graphics` pode ser usado **sem** a feature `wgpu`: ele abre a janela em OpenGL e mostra um diálogo nativo explicando a falha se não houver OpenGL. Nunca ativar a feature `wgpu` dele.
  - Garantia em CI: `cargo deny` banindo os crates `wgpu`, `wgpu-core`, `wgpu-hal` e `ash`, para que nenhuma dependência os traga de volta.
- Verificação em CI: rodar a suíte de testes e o binário headless sob `qemu-x86_64 -cpu core2duo` (sem SSE4.2/AVX). Se aparecer `SIGILL`, o CI quebra.
- Opcional/stretch: build `i686` (32 bits) e `aarch64` para placas ARM.

### 0.2 Baixo uso de RAM

Metas (RSS medido no Linux, binário release):

| Cenário | Meta |
|---|---|
| GUI ociosa, sem sessão | ≤ 50 MB |
| Sessão com 500 mensagens e saídas de ferramentas grandes | ≤ 120 MB |
| `magia-cli` headless executando um turno | ≤ 25 MB |

Táticas:
- **Histórico fora da RAM:** a sessão vive no JSONL em disco; a memória mantém só um índice (offset por mensagem) + a janela visível + o contexto que será enviado. Saídas de ferramentas grandes vão inteiras para disco e ficam truncadas em memória.
- **UI virtualizada:** `ScrollArea::show_viewport` para só fazer layout das mensagens visíveis; cache de altura por mensagem, e galleys descartados fora da tela.
- **Fontes:** só Inter/fonte do sistema + fallback latino. Fallbacks CJK/emoji (fontes de dezenas de MB) carregados **sob demanda**, apenas quando aparece um caractere desses scripts. `fastframe-emoji` desligado por padrão.
- **Syntax highlight sob demanda:** `syntect` carregando só a sintaxe de cada bloco, preguiçosamente, e só para blocos visíveis; o tema/sintaxe default completo nunca é carregado. Feature `highlight` desligável.
- **Sem tokenizer local:** nada de `tiktoken-rs` (tabelas BPE ocupam dezenas de MB). Usar o `usage` retornado pela API e uma estimativa `bytes/4` para decidir a compactação.
- **Tokio enxuto:** runtime multi-thread com 2 workers (`worker_threads(2)`) e `max_blocking_threads` baixo; buffers de canais e de stdout do `bash` limitados (backpressure).
- **Imagens anexadas:** reduzidas na hora de anexar, sem manter o original decodificado.
- **Processos externos sob demanda:** servidores MCP só sobem quando uma ferramenta deles é usada e são encerrados quando ficam ociosos.
- Perfil release: `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
- **Medição em CI:** benchmark com `/usr/bin/time -v` (Maximum resident set size) sobre uma sessão sintética; regressão acima de 10% quebra o build.

---

## 1. Features de harnesses modernos (Claude Code, OpenCode, Codex CLI, Aider, Goose)

Agrupadas por prioridade para o magia:

**Núcleo (MVP)**
- Loop agêntico: modelo → tool calls (inclusive paralelas) → resultados → modelo, até `stop`.
- Streaming token a token (SSE), com cancelamento (Esc/stop) a qualquer momento.
- Ferramentas básicas: `read`, `write`, `edit` (substituição exata de string), `bash` (com timeout e saída truncada), `grep`, `glob`, `ls`, `webfetch`, `todo`.
- Sistema de permissões: regras allow/ask/deny por ferramenta + padrão (ex.: `bash(git status*)`), modos *ask*, *auto-edit*, *auto* (estilo auto mode do Claude Code: classificador local + modelo revisor), *plan* (somente leitura) e *yolo*.
- Troca de provedor/modelo no meio da sessão; seletor de esforço de raciocínio.
- Contagem de tokens e custo por turno/sessão.

**Contexto e sessões**
- Arquivo de instruções do projeto (`AGENTS.md`, com fallback para `CLAUDE.md`) injetado no system prompt.
- Agentes primários estilo OpenCode: *Build*, *Plan* e um por `.md` da raiz do projeto (menos README/LICENSE/…), cujo texto é **acrescentado** ao prompt base; seletor na GUI, `--agent` no CLI, `default_agent` na config. A compactação automática desconta o prompt fixo.
- Persistência de sessões, retomar, bifurcar (fork) a partir de qualquer mensagem.
- Compactação automática (resumo do histórico ao se aproximar do limite de contexto) + compactação manual.
- Prompt caching (`cache_control` no formato Anthropic; OpenRouter repassa).
- Menções `@arquivo` e anexos de imagem.

**Extensibilidade**
- Cliente MCP (stdio e HTTP streamable), ferramentas MCP namespaced `mcp__server__tool`.
- Slash commands customizados (arquivos markdown em `.magia/commands/`).
- Skills (pasta com `SKILL.md` + recursos, carregadas sob demanda pela descrição).
- Subagentes (sessão filha com contexto próprio e conjunto de ferramentas restrito, retorna só o resultado).
- Hooks (comandos shell em eventos: `pre_tool`, `post_tool`, `user_prompt`, `stop`), que podem bloquear/modificar.

**Segurança e qualidade de vida**
- Checkpoints/undo: snapshot dos arquivos antes de cada edição (repo git sombra), "rewind" para qualquer turno.
- Revisão de diff na UI antes/depois de aplicar edições.
- Sandbox opcional para `bash` (Linux: bubblewrap/landlock).
- Diagnósticos LSP após edições (fase tardia).
- Modo headless (`magia <pedido>`, igual a `--run`) para scripts/CI, saída JSON.
- Múltiplas sessões em abas.

---

## 2. Arquitetura

Workspace Cargo, com o core sem nenhuma dependência de UI:

```
crates/
  magia-types      # Mensagens, ContentBlock, ToolCall, Usage, eventos. Sem I/O.
  magia-providers  # trait Provider + adaptadores de wire format + SSE
  magia-tools      # trait Tool, ferramentas embutidas, motor de permissões
  magia-mcp        # cliente MCP → expõe ferramentas como `dyn Tool`
  magia-core       # Agent loop, Session, compactação, hooks, subagentes, config
  magia-store      # persistência de sessões/checkpoints
apps/
  magia-gui        # eframe + fastframe
  magia-cli        # headless (mesmo core)
```

### 2.1 Provedores

Distinguir **wire format** de **provedor**:

```rust
enum Wire { ChatCompletions, Responses, AnthropicMessages }

struct Endpoint { base_url: Url, wire: Wire, auth: Auth, extra_headers: HeaderMap }

#[async_trait]
trait Provider: Send + Sync {
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    fn stream(&self, req: ChatRequest) -> BoxStream<'static, Result<StreamEvent>>;
}
```

- Um adaptador por `Wire` (3 no total) converte o `ChatRequest` interno ↔ JSON do fio e normaliza os eventos SSE em `StreamEvent { TextDelta, ReasoningDelta, ToolCallStart, ToolCallArgsDelta, ToolCallEnd, Usage, Done }`.
- Zen, Go e OpenRouter são **configurações** (base URL + escolha de `Wire` por modelo), não código novo. Regra: modelos `gpt-*` no Zen → `Responses`; `claude-*` (e os listados pelo Go como Messages-only) → `AnthropicMessages`; o resto → `ChatCompletions`. Permitir override por modelo na config.
- OpenRouter: enviar `HTTP-Referer`/`X-Title`, suportar `provider` routing e `reasoning` no corpo.
- Retry no agente (`Agent::run_loop`), não no `Client`: qualquer falha recuperável — conexão, 408/409/425/429/5xx, erro de decode, evento de erro do provedor no meio do SSE, stream que termina sem `finish_reason`/`[DONE]` — espera 5 s (ou o `retry-after`, até 60 s) e repete o request, até 10 vezes, cancelável. Cota esgotada, contexto longo demais e 4xx não repetem. Texto parcial já mostrado é descartado (`AgentEvent::Retry { discarded }`). A GUI mostra "trying again in Ns"; o CLI imprime a tentativa.
- Fila na GUI: Enter durante um turno enfileira a mensagem (lista acima do composer, × remove); ao terminar, todas vão juntas num só pedido, separadas por linha em branco, com as imagens. Esc/Parar cancela e devolve a fila ao composer.
- `base_url` e headers extras sobrescrevíveis por provedor na config.

### 2.2 Agent loop e concorrência com egui

- O egui é immediate-mode e roda na thread principal; **nada de I/O no `update()`**.
- Um runtime `tokio` multi-thread é criado em `main` e roda ao lado do eframe.
- UI → core: `mpsc::UnboundedSender<Command>` (`SendPrompt`, `Cancel`, `ApprovePermission(id, decision)`, `SwitchModel`…).
- Core → UI: `mpsc::UnboundedReceiver<AgentEvent>`, drenado com `try_recv()` a cada frame; o core chama `ctx.request_repaint()` (clone do `egui::Context`) ao emitir eventos.
- Pedido de permissão = evento `PermissionRequest { id, tool, input }` + `oneshot` aguardando a resposta da UI.
- Cancelamento via `CancellationToken` (tokio-util) propagado para o stream HTTP e processos filhos de `bash`.
- Tool calls independentes executam em paralelo (`join_all`); escritas no mesmo arquivo serializadas.

### 2.3 Ferramentas e permissões

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;   // JSON Schema (gerado com schemars)
    fn read_only(&self) -> bool;
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<ToolOutput>;
}
```

- `edit` exige ter lido o arquivo antes na sessão e `old_string` único (mesmo contrato do Claude Code — reduz edições erradas).
- Saídas grandes são truncadas com aviso (íntegra salva em disco).
- Tool call para ferramenta desconhecida vira erro de ferramenta devolvido ao modelo, sem quebrar o loop.
- Motor de permissões avalia regras da config (usuário → projeto → sessão), em ordem deny > ask > allow.

### 2.4 Persistência ("Rust puro")

Evitar dependências C: `reqwest` com `rustls` + provider `ring` (sem OpenSSL), sessões em **JSONL append-only** (um arquivo por sessão, eventos imutáveis — fork = novo arquivo apontando para pai+offset) ou `redb` para índice. Sem SQLite/rusqlite. Caminhos via `fastframe-dirs`, escrita atômica via `fastframe-file`.

Config em **JSON/JSONC compatível com OpenCode** (`magia.json[c]`: `~/.config/magia/`, projeto, `.magia/`, `$MAGIA_CONFIG`; camadas fundidas chave a chave; `{env:X}`/`{file:p}`). Configurações só do magia ficam sob `"magia"`. Chaves de API via env/`{env:}`; keyring do sistema não implementado.

### 2.5 GUI (eframe + fastframe)

Layout: barra lateral (sessões/projetos) | conversa (mensagens com markdown, blocos de tool call colapsáveis, diffs) | painel direito opcional (todo, arquivos alterados, custo/tokens). Composer na base com seletor de modelo, modo de permissão e esforço.

Crates fastframe úteis (todos pinados na **mesma tag**, hoje `v0.17.0`, via git — não estão no crates.io):

| Crate | Uso no magia |
|---|---|
| `fastframe-fonts`, `fastframe-text` | fontes e renderização nítida (fallbacks CJK só sob demanda, ver §0.2) |
| `fastframe-theme` | paletas JSON, tema claro/escuro do sistema |
| `fastframe-icons` | ícones Lucide |
| `fastframe-titlebar` | barra de título customizada |
| `fastframe-dirs`, `fastframe-file` | caminhos de config/estado, escrita atômica |
| `fastframe-log` | logs por execução |
| `fastframe-notifications` | aviso quando o agente termina / pede permissão |
| `fastframe-instance` | instância única |
| `fastframe-tray`, `fastframe-shell` | rodar em segundo plano (fase tardia) |
| `fastframe-update` | atualizações assinadas (fase tardia) |

`fastframe-graphics` entra **sem** a feature `wgpu` (fallback de diagnóstico do OpenGL). `fastframe-emoji` fica desligado por padrão (§0).

Markdown: `egui_commonmark`; syntax highlight: `syntect` com `fancy-regex` (puro Rust, carregamento preguiçoso); diff: `similar`.

Requisitos: **Rust 1.98+** e **egui/eframe 0.36.2** (exigidos pelo fastframe), eframe com `default-features = false, features = ["glow", ...]`.

---

## 3. Fases

| Fase | Entrega | Critério de pronto |
|---|---|---|
| **F0 — Esqueleto** | Workspace, `magia-types`, janela eframe (`glow`) com fastframe-fonts/text/theme, config JSON/JSONC (compatível com OpenCode), CI (fmt, clippy `-D warnings`, test, **testes sob `qemu -cpu core2duo`**, **benchmark de RSS**) | `cargo run -p magia-gui` abre janela temada; GUI ociosa ≤ 50 MB; roda em llvmpipe |
| **F1 — Chat** | `magia-providers` com os 3 wires + SSE, presets Zen/Go/OpenRouter, listagem de modelos, chat com streaming e cancelamento na GUI | Conversa streaming com um modelo de cada provedor; testes de parsing SSE com fixtures gravadas |
| **F2 — Agente** | `magia-tools` (read/write/edit/bash/grep/glob/ls/todo), loop agêntico, tool calls paralelas, diálogo de permissão, modo plan | Agente edita arquivos de um projeto de teste com aprovação na UI |
| **F3 — Contexto** | Sessões JSONL com histórico paginado do disco (retomar/fork), UI virtualizada, `AGENTS.md`, `@arquivo`, compactação automática, contagem de tokens/custo, prompt caching | Sessão longa sobrevive a reinício e a estouro de contexto |
| **F4 — Extensões** | Cliente MCP (`rmcp`), slash commands, skills, hooks | Servidor MCP stdio de exemplo funcionando |
| **F5 — Avançado** | Subagentes, checkpoints/rewind, revisão de diff, `magia-cli` headless, sandbox bash | Rewind restaura arquivos; `magia <pedido>` em CI |
| **F6 — Polimento** | Abas de sessão, notificações, tray, LSP, auto-update | — |

## 3.1 Modo Magia (ultrawork próprio)

Terceiro agente embutido, ao lado de Build e Plan. Ideia (mesma do "ultrawork" do oh-my-opencode, implementação própria): o turno não termina quando o modelo para de chamar ferramentas se ainda há trabalho; o core injeta uma mensagem de continuação e roda outro *round*, até a lista de tarefas fechar e uma revisão independente aprovar. O limite de 50 passos passa a valer por round, não por pedido.

**Decisões**
- É um agente (`agents::MAGIA`), não um `PermissionMode`: combina com Ask/Accept edits/Auto/Yolo. No modo de permissão Plan o loop fica desligado (nada pode ser alterado). Subagentes nunca fazem loop.
- Tudo dentro de **um** `Agent::run_turn`: um `TurnDone` no fim, o cancelamento para tudo, um checkpoint por pedido (o `/rewind 1` desfaz o pedido inteiro com todos os rounds).
- Critério de "acabou": nenhum todo `pending`/`in_progress`. Novo status `cancelled` para o modelo encerrar com honestidade o que decidiu não fazer (em vez de marcar `completed`).
- A mensagem de continuação é `Message::User` com o prefixo `[magia: continue]`, e traz o pedido original (até 2000 caracteres) e a lista de tarefas atual, então sobrevive à compactação. `rewind`, o histórico do composer e a reconstrução do transcript ignoram ou mostram essas mensagens à parte.
- **Paradas**: tudo concluído (e revisão aprovada ou desligada); round sem nenhuma chamada de ferramenta (o modelo só falou: precisa do usuário ou acha que acabou); 3 rounds seguidos sem mudar a lista de tarefas nem arquivos; o usuário negou uma ferramenta no round (ele está presente e disse não); erro do provedor; cancelamento; `max_rounds` (padrão 30).
- Bater o limite de passos no meio de um round não é erro no modo Magia: vira fim de round e o loop decide se continua.
- **Revisão independente** (o "Oracle"): quando as tarefas fecham e o pedido alterou arquivos, um subagente só-leitura recebe o pedido original, a lista de tarefas, os arquivos alterados (checkpoints) e a última resposta, e responde JSON `{"verdict":"pass"|"fail","issues":[…]}`. `fail` vira uma continuação com os problemas; no máximo 2 revisões por pedido; resposta ilegível conta como `pass` (aviso), para não prender o loop.
- Config sob `"magia"`: `"magia_mode": {"max_rounds": 30, "review": true}`.

Também: as continuações ficam fora do revisor do modo Auto (`review::user_requests`), porque carregam a lista de tarefas escrita pelo modelo. "Progresso" = a lista mudou ou houve um `write`/`edit` bem-sucedido no round.

**Etapas** (todas feitas em 2026-11-12; chat real ainda não testado, coberto pelo servidor falso)
1. Core: `agents::MAGIA`, instruções do modo (`magia_mode.rs`: planejar com `todo`, trabalhar sem pedir licença, `question` só para decisões do usuário, verificar no fim, negação é final), campo `Agent::magia` com a config.
2. `todo`: status `cancelled` (schema, descrição, GUI riscado/esmaecido).
3. Loop: `run_turn` em rounds; mensagem de continuação; condições de parada; novo `AgentEvent::Round { n, reason }`.
4. Limite de passos vira fim de round no modo Magia.
5. Higiene do histórico: `rewind` e `sent_messages` ignoram continuações; `items_from_history` as mostra como divisória.
6. Revisão independente (subagente revisor com veredito JSON).
7. Config `magia.magia_mode` + `docs/magia.example.jsonc` + teste de config.
8. Front-ends: "Magia" no seletor de agentes da GUI (sai do Plan como o Build), divisória "↻ round N · motivo" no transcript; CLI `--agent magia` imprime os rounds.
9. Visibilidade: `AgentEvent::Magia(MagiaPhase)` (Working/Reviewing/ReviewFailed/Finished com motivo) e `AgentEvent::SubTool { parent, name, input }` (cada ferramenta de um subagente `task` ou do revisor, `parent` = id do call ou `"review"`, repassada ao vivo). A GUI junta isso em `activity.rs`: a barra lateral mostra "Magia round N" + fase + problemas da revisão, e "Subagents" (rótulo, passos, última ferramenta, resultado) acima de "Tasks"; em janela estreita vai no cartão colapsável. O CLI imprime `↳ subagent:`/`↳ reviewer:` e o motivo final.
9. Testes com o servidor SSE falso: continua com tarefas pendentes; para sem progresso; para quando o usuário nega; limite de passos continua; revisão `fail` reabre e `pass` encerra; rewind ignora continuações; Build não muda.
10. Docs: status aqui e no CLAUDE.md.

## 4. Riscos / decisões em aberto

- **Rust 1.98+**: verificar toolchain disponível; o fastframe é jovem e a API pode quebrar entre tags — atualizar todas as crates fastframe juntas.
- **Formatos do Zen/Go por modelo** mudam; manter o mapeamento modelo→wire na config, com detecção pelo `/models` quando possível.
- **Contagem de tokens**: sem tokenizer local (custo de RAM); `usage` da API é a fonte de verdade e `bytes/4` a estimativa — pode compactar um pouco cedo/tarde demais; margem de segurança de 15%.
- **fastframe e CPUs antigas**: auditar cada crate fastframe adotada (e o fork de egui/winit, se usado) com o teste em QEMU antes de fixar a tag.
- **OpenGL muito antigo** (GPUs pré-2008): validar o `glow` com o `llvmpipe` como caminho garantido; a renderização em software custa CPU, então limitar o repaint a eventos (sem animações contínuas).

---

## 5. Status (2026-11-11)

| Fase | Estado |
|---|---|
| F0 Esqueleto | ✅ workspace, config JSON/JSONC (compatível com OpenCode), eframe `glow` + fastframe-fonts/text/graphics/notifications (sem wgpu), CI em `.github/workflows/ci.yml` |
| F1 Chat | ✅ 3 wires + SSE + retry + prompt caching (Messages); `/models` validado contra Zen, Go e OpenRouter reais; **chat real não testado (sem API key)** — coberto por fixtures e servidor falso |
| F2 Agente | ✅ read/write/edit/bash/ls/glob/grep/todo, modos ask/accept_edits/auto/plan/yolo, aprovação na GUI/CLI, cancelamento, leituras em paralelo |
| F3 Contexto | ✅ sessões JSONL por pasta + `/resume` com seletor e busca (todas as pastas; só restaura na própria) + fork, `AGENTS.md`/`CLAUDE.md`, `@arquivo`, imagens (Ctrl+V, arrastar, `@foto.png`; testado no Go com kimi-k3, claude-haiku-5-5, gpt-5.6-luna), compactação automática e `/compact`, GUI virtualizada. Fora: custo em $ (Zen não publica preços em `/models`), histórico paginado do disco (a GUI ainda mantém o transcript em RAM; só o desenho é virtualizado) |
| F4 Extensões | ✅ MCP stdio, skills, slash commands (com autocomplete `/` na GUI e `magia --commands` no CLI), hooks |
| F5 Avançado | ✅ subagente `task`, checkpoints + `/rewind` (só `write`/`edit`; `bash` não é rastreado), sandbox `bwrap`, CLI headless. Fora: diff visual antes de aplicar edição |
| F6 Polimento | 🟡 notificações desktop (lazy, só com a janela sem foco); identidade visual com tokens claro/escuro seguindo o sistema (`theme.rs`, contraste WCAG testado), coluna de leitura, markdown nas respostas, ferramentas como linhas de log, modais de permissão/pergunta, tela inicial com atalhos. Fora: abas, tray, LSP, auto-update, MCP via HTTP |

Medições (release, x86-64 baseline = sse2): CLI 4,5 MB / 8 MB RSS; GUI ≈20 MB anon (+≈49 MB de bibliotecas do sistema) com GPU (19,4 MB após o redesign, com sessão aberta), ≈43 MB anon sob llvmpipe. CPU antiga: `scripts/check-baseline.sh` passa no CLI (85 funções pós-SSE2, todas em ring/memchr/aho-corasick com detecção em runtime); qemu não pôde ser instalado (sem sudo), então não houve execução real em CPU sem AVX.

Config JSON: implementados `model`, `provider(s)` (baseURL/apiKey/headers/npm/env/models), `mcp` local, `permission(s)` com padrões, `instructions` (arquivos/globs), `command(s)`. Ignorados: `agent(s)`, `lsp`, `formatter`, `plugin`, `mcp` remoto (aviso), `skills.urls`, URLs em `instructions`.

Modo Magia (2026-11-12): agente `magia` (GUI, `--agent magia` no CLI) que trabalha em rounds até a lista de tarefas fechar, com revisão independente dos arquivos alterados; detalhes em §3.1. Seletor de modelos da GUI agora é um botão com lista pesquisável (`model_picker.rs`; Enter sem resultado usa o id digitado).

Ferramentas (2026-11-11): adicionadas `webfetch` (HTML→texto próprio, corpo limitado a 2 MB), `websearch` (página HTML do DuckDuckGo; pode exigir captcha sob uso intenso) e `question`; a lista enviada ao modelo agora é filtrada pelo modo, pelas regras e por ser subagente.
