# opencode-compact-if-big

> Pede compactação de sessão no **seu** momento, não no pior momento.
> A helper that triggers OpenCode session compaction at a threshold you choose — with guards
> against compacting while work is in flight.

**Idioma:** pt-BR (o autor) · short English abstract at the end.

---

## O problema

O OpenCode v2 **não tem uma chave de limite de contexto** no `opencode.jsonc`. O
`compaction.auto: true` só dispara quando a sessão **se aproxima do limite do modelo** — ou seja,
**no meio de uma tarefa longa**, que é o pior momento possível. E compactação é **lossy e
irreversível**.

Medições reais do autor (sessões de agente com DeepSeek V4.1 Flash, contexto de 1M):

- o custo de tempo é `nº de requisições (steps) × pedágio`, com **pedágio de ~20 s por requisição**
  (entregar 100 ou 3.000 tokens no mesmo step custa o mesmo);
- **75% do tempo de parede** dos turnos é esse pedágio;
- a mediana de latência **satura** (~11–15 s) acima de ~150–250k tokens de contexto, mas a **cauda**
  piora: p90 de **45,6 s** (100–150k) a **149,6 s** (600–900k).

Logo: compactar ajuda, mas **quando** compactar é decisão sua — e nunca no meio de uma tarefa.

## O que faz

| Recurso | Detalhe |
|---|---|
| **Mede o contexto real** | `tokens.input + tokens.cache.read` da última requisição da sessão |
| **Gatilho por teto** | `--above 600k` (aceita `600k`, `1M`, `900000`) |
| **TRAVAS contra compactar em voo** | recusa quando: `finish='tool-calls'` (turno em andamento), **subagente ativo** na janela, ou prompts na fila |
| **Dry-run é o padrão** | nada é enviado sem `--apply` (na TUI: sem armar com `a`) |
| **TUI (curses)** | lista com contexto/estado/idade, auto-refresh, teto ajustável na hora |
| **Sem dependências** | python3 + stdlib (`curses` incluído no Linux/macOS) |

## Instalação

```bash
git clone https://github.com/marcos-toliveira/opencode-compact-if-big
cd opencode-compact-if-big
./install.sh          # copia para ~/.local/bin
```

Requisitos: `python3` 3.8+ e o binário `opencode` no `PATH`.

## Uso

```bash
opencode-compact-if-big --list                    # relatório: contexto, estado e idade
opencode-compact-if-big                           # dry-run: diz o que faria
opencode-compact-if-big --above 600k --apply      # pede a compactação (se a sessão estiver livre)
opencode-compact-if-big --session ses_xxx --apply # sessão específica
opencode-compact-if-big --tui                     # interface interativa
opencode-compact-if-big --no-titles --list        # sem títulos (privacidade)
```

### TUI

```
↑/↓ mover · r atualizar · a armar/aplicar de verdade · +/- teto · o ordenar · t todas
c compactar (com confirmação) · C forçar (exige digitar "force") · q sair
```

Ela começa em **SIMULAÇÃO**; `a` arma de verdade. `C` (forçar) só é aceito digitando `force` no
prompt — porque compactação é irreversível.

## Travas (por que é seguro)

Compactar no meio de uma tarefa pode destruir exatamente o estado que a tarefa usa. O utilitário
recusa quando detecta **trabalho em voo**:

1. **turno em andamento** — a última mensagem do assistente tem `finish='tool-calls'`;
2. **subagente ativo** — alguma sessão-filha com atividade nos últimos `--idle-window` segundos
   (padrão 600) **e** posterior ao último item do pai;
3. **fila** — registros em `session_pending` / `session_inbox`.

> Caso real que originou a trava: uma sessão estava "aguardando subagentes" com `finish='stop'` —
> o sinal de turno **não** a pegava; **só a atividade das sessões-filhas** a classifica como em voo.

`--force` existe para casos deliberados, mas exige confirmação extra.

## Como funciona por dentro

- Lê o banco **local** do OpenCode em modo somente leitura (`session_v2`, `session_message`,
  `session_pending`, `session_inbox`).
- Pede a compactação por `POST /api/session/<id>/compact` (via `opencode api`). A API executa no
  **próximo ponto seguro** (step boundary), **funde** pedidos repetidos e roda mesmo com
  `compaction.auto: false`.

### Instâncias isoladas (ex.: `opencode-2`)

Se você roda uma **segunda instância** com `XDG_*` próprios (wrapper `opencode-2`), as duas pontas
precisam apontar para a **mesma** instância — o banco **e** o binário que fala com a API:

```bash
OPENCODE_COMPACT_DB=~/.opencode-go2/data/opencode/opencode.db \
OPENCODE_COMPACT_BIN=opencode-2 opencode-compact-if-big --above 600k --apply
```

| Flag / env | Para quê |
|---|---|
| `--db` / `$OPENCODE_COMPACT_DB` | qual banco ler (onde estão as sessões) |
| `--bin` / `$OPENCODE_COMPACT_BIN` | qual binário/instância fala com a API (padrão `opencode`) |

### Ordem de resolução do banco

1. `--db CAMINHO`
2. `$OPENCODE_COMPACT_DB`
3. `opencode debug paths` (campo `db`)
4. padrões por SO (Linux/macOS/Windows)

## Privacidade

- Lê **apenas metadados**: id, título, diretório, contagem de tokens, `finish`, relações
  pai/filho e tamanho das filas.
- **Não** lê credenciais (`account`/`credential`) e **não** imprime conteúdo de conversa.
- Títulos de sessão e caminhos podem conter informação sensível (nome de projeto, domínio): use
  `--no-titles` antes de compartilhar saídas ou screenshots.
- Nada sai da sua máquina: a única chamada de rede é para o **seu** servidor OpenCode local.

## Compatibilidade

Testado com **opencode v2.0.20 (beta)**. O OpenCode é atualizado com frequência e o schema do banco
pode mudar entre versões; quando isso acontece, o utilitário **falha com mensagem clara** (em vez de
reportar números errados). Se quebrar, abra uma issue com a saída de `opencode --version` e de
`opencode debug paths`.

> Nota: o canal `latest` do npm é a **linha estável** da v2 — a cadência é rápida (releases finais,
> não nightly). O pacote AUR se chama `opencode-beta` por legado.

## Testes

```bash
python3 -m unittest discover -s tests -v
```

Os testes constroem um banco SQLite temporário com o schema mínimo e cobrem as três travas, o caso
"subagente antigo não marca em voo", o modo dry-run (que **não** chama a API) e o `--no-titles`.

## Aviso

Compactar é **lossy**: o resumo substitui a conversa antiga. A ferramenta existe para você escolher
o momento — não para decidir por você. Se você usa [ai-memory](https://github.com/akitaonrails/ai-memory),
os hooks `pre-compact`/`session-start` dão uma rede de segurança (o detalhe continua consultável na
memória).

## Licença

MIT — ver [LICENSE](LICENSE).

---

### English abstract

OpenCode v2 has no context-threshold setting for compaction: `compaction.auto` fires near the model
limit — the worst possible moment — and compaction is lossy. This tool triggers compaction when
*you* want it (end of a work block), measuring the last request's context
(`input + cache.read`), and **refuses** to act when work is in flight (in-progress turn, active
sub-agents, queued prompts). Dry-run is the default; a curses TUI is included. Metadata-only access
to the local OpenCode DB; `--no-titles` for sharing. MIT.
