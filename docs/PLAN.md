# ufrh — Plano de implementação

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
| `ufrh-cli` headless executando um turno | ≤ 25 MB |

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

Agrupadas por prioridade para o ufrh:

**Núcleo (MVP)**
- Loop agêntico: modelo → tool calls (inclusive paralelas) → resultados → modelo, até `stop`.
- Streaming token a token (SSE), com cancelamento (Esc/stop) a qualquer momento.
- Ferramentas básicas: `read`, `write`, `edit` (substituição exata de string), `bash` (com timeout e saída truncada), `grep`, `glob`, `ls`, `webfetch`, `todo`.
- Sistema de permissões: regras allow/ask/deny por ferramenta + padrão (ex.: `bash(git status*)`), modos *ask*, *auto-edit*, *plan* (somente leitura) e *yolo*.
- Troca de provedor/modelo no meio da sessão; seletor de esforço de raciocínio.
- Contagem de tokens e custo por turno/sessão.

**Contexto e sessões**
- Arquivo de instruções do projeto (`AGENTS.md`, com fallback para `CLAUDE.md`) injetado no system prompt.
- Persistência de sessões, retomar, bifurcar (fork) a partir de qualquer mensagem.
- Compactação automática (resumo do histórico ao se aproximar do limite de contexto) + compactação manual.
- Prompt caching (`cache_control` no formato Anthropic; OpenRouter repassa).
- Menções `@arquivo` e anexos de imagem.

**Extensibilidade**
- Cliente MCP (stdio e HTTP streamable), ferramentas MCP namespaced `mcp__server__tool`.
- Slash commands customizados (arquivos markdown em `.ufrh/commands/`).
- Skills (pasta com `SKILL.md` + recursos, carregadas sob demanda pela descrição).
- Subagentes (sessão filha com contexto próprio e conjunto de ferramentas restrito, retorna só o resultado).
- Hooks (comandos shell em eventos: `pre_tool`, `post_tool`, `user_prompt`, `stop`), que podem bloquear/modificar.

**Segurança e qualidade de vida**
- Checkpoints/undo: snapshot dos arquivos antes de cada edição (repo git sombra), "rewind" para qualquer turno.
- Revisão de diff na UI antes/depois de aplicar edições.
- Sandbox opcional para `bash` (Linux: bubblewrap/landlock).
- Diagnósticos LSP após edições (fase tardia).
- Modo headless (`ufrh run "prompt"`) para scripts/CI, saída JSON.
- Múltiplas sessões em abas.

---

## 2. Arquitetura

Workspace Cargo, com o core sem nenhuma dependência de UI:

```
crates/
  ufrh-types      # Mensagens, ContentBlock, ToolCall, Usage, eventos. Sem I/O.
  ufrh-providers  # trait Provider + adaptadores de wire format + SSE
  ufrh-tools      # trait Tool, ferramentas embutidas, motor de permissões
  ufrh-mcp        # cliente MCP → expõe ferramentas como `dyn Tool`
  ufrh-core       # Agent loop, Session, compactação, hooks, subagentes, config
  ufrh-store      # persistência de sessões/checkpoints
apps/
  ufrh-gui        # eframe + fastframe
  ufrh-cli        # headless (mesmo core)
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
- Retry com backoff exponencial em 429/5xx/erros de conexão, respeitando `retry-after`.
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

Config em TOML: `~/.config/ufrh/config.toml` (usuário) + `.ufrh/config.toml` (projeto). Chaves de API via env ou keyring do sistema (`keyring` crate), nunca no TOML do projeto.

### 2.5 GUI (eframe + fastframe)

Layout: barra lateral (sessões/projetos) | conversa (mensagens com markdown, blocos de tool call colapsáveis, diffs) | painel direito opcional (todo, arquivos alterados, custo/tokens). Composer na base com seletor de modelo, modo de permissão e esforço.

Crates fastframe úteis (todos pinados na **mesma tag**, hoje `v0.17.0`, via git — não estão no crates.io):

| Crate | Uso no ufrh |
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
| **F0 — Esqueleto** | Workspace, `ufrh-types`, janela eframe (`glow`) com fastframe-fonts/text/theme, config TOML, CI (fmt, clippy `-D warnings`, test, **testes sob `qemu -cpu core2duo`**, **benchmark de RSS**) | `cargo run -p ufrh-gui` abre janela temada; GUI ociosa ≤ 50 MB; roda em llvmpipe |
| **F1 — Chat** | `ufrh-providers` com os 3 wires + SSE, presets Zen/Go/OpenRouter, listagem de modelos, chat com streaming e cancelamento na GUI | Conversa streaming com um modelo de cada provedor; testes de parsing SSE com fixtures gravadas |
| **F2 — Agente** | `ufrh-tools` (read/write/edit/bash/grep/glob/ls/todo), loop agêntico, tool calls paralelas, diálogo de permissão, modo plan | Agente edita arquivos de um projeto de teste com aprovação na UI |
| **F3 — Contexto** | Sessões JSONL com histórico paginado do disco (retomar/fork), UI virtualizada, `AGENTS.md`, `@arquivo`, compactação automática, contagem de tokens/custo, prompt caching | Sessão longa sobrevive a reinício e a estouro de contexto |
| **F4 — Extensões** | Cliente MCP (`rmcp`), slash commands, skills, hooks | Servidor MCP stdio de exemplo funcionando |
| **F5 — Avançado** | Subagentes, checkpoints/rewind, revisão de diff, `ufrh-cli` headless, sandbox bash | Rewind restaura arquivos; `ufrh run` em CI |
| **F6 — Polimento** | Abas de sessão, notificações, tray, LSP, auto-update | — |

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
| F0 Esqueleto | ✅ workspace, config TOML, eframe `glow` + fastframe-fonts/text/graphics/notifications (sem wgpu), CI em `.github/workflows/ci.yml` |
| F1 Chat | ✅ 3 wires + SSE + retry + prompt caching (Messages); `/models` validado contra Zen, Go e OpenRouter reais; **chat real não testado (sem API key)** — coberto por fixtures e servidor falso |
| F2 Agente | ✅ read/write/edit/bash/ls/glob/grep/todo, modos ask/accept_edits/plan/yolo, aprovação na GUI/CLI, cancelamento, leituras em paralelo |
| F3 Contexto | ✅ sessões JSONL + resume/fork, `AGENTS.md`/`CLAUDE.md`, `@arquivo`, compactação automática e `/compact`, GUI virtualizada. Fora: custo em $ (Zen não publica preços em `/models`), histórico paginado do disco (a GUI ainda mantém o transcript em RAM; só o desenho é virtualizado) |
| F4 Extensões | ✅ MCP stdio, skills, slash commands, hooks |
| F5 Avançado | ✅ subagente `task`, checkpoints + `/rewind` (só `write`/`edit`; `bash` não é rastreado), sandbox `bwrap`, CLI headless. Fora: diff visual antes de aplicar edição |
| F6 Polimento | 🟡 notificações desktop (lazy, só com a janela sem foco). Fora: abas, tray, LSP, auto-update, MCP via HTTP |

Medições (release, x86-64 baseline = sse2): CLI 4,5 MB / 8 MB RSS; GUI ≈20 MB anon (+≈49 MB de bibliotecas do sistema) com GPU, ≈43 MB anon sob llvmpipe. CPU antiga: `scripts/check-baseline.sh` passa no CLI (85 funções pós-SSE2, todas em ring/memchr/aho-corasick com detecção em runtime); qemu não pôde ser instalado (sem sudo), então não houve execução real em CPU sem AVX.
