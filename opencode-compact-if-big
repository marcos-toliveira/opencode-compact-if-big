#!/usr/bin/env python3
"""opencode-compact-if-big — pede compactação quando o contexto passa do teto.

PROBLEMA: o OpenCode v2 **não tem chave de limite de contexto** no config. O `compaction.auto`
só dispara ao **aproximar do limite do modelo** — ou seja, no pior momento: no meio de uma
tarefa longa. E a compactação é **lossy e irreversível**.

SOLUÇÃO: um gatilho explícito, no **seu** momento (fim de bloco de trabalho), com **travas**
para não compactar quando há trabalho em voo.

MEDIDA: contexto da última requisição da sessão = `tokens.input + tokens.cache.read`.

TRAVAS (o ponto crítico): o utilitário **recusa** agir quando a sessão tem trabalho em voo:
  (a) a última mensagem do assistente é `finish='tool-calls'` (turno em andamento);
  (b) existe **subagente (sessão-filha) com atividade recente** (janela `--idle-window`);
  (c) há **prompts na fila** (`session_pending` / `session_inbox`).
Só `--force` passa por cima — e exige confirmação explícita na TUI.

USO (CLI):
  opencode-compact-if-big                          # DRY-RUN: mostra sessões, tamanho e estado
  opencode-compact-if-big --list                   # só o relatório
  opencode-compact-if-big --above 600k --apply     # pede a compactação (se não houver trabalho em voo)
  opencode-compact-if-big --session ses_xxx --apply
  opencode-compact-if-big --session ses_xxx --apply --force

USO EM INSTÂNCIA ISOLADA (ex.: opencode-2, com XDG próprio) — as duas pontas precisam apontar
para a MESMA instância (banco + binário que fala com a API):
  OPENCODE_COMPACT_DB=~/.opencode-go2/data/opencode/opencode.db \
  OPENCODE_COMPACT_BIN=opencode-2 opencode-compact-if-big --above 600k --apply

USO (TUI — curses, sem dependências):
  opencode-compact-if-big --tui
  ↑/↓ mover · r atualizar · a armar/aplicar de verdade · +/- teto · o ordenar · t todas
  c pedir compactação (com confirmação) · C forçar (exige digitar "force") · q sair

PRIVACIDADE: lê **apenas metadados** do banco local (`session_v2`, `session_message`,
`session_pending`, `session_inbox`). **Não** lê credenciais (`account`/`credential`) nem
imprime conteúdo de conversa. Títulos e caminhos podem ser ocultados com `--no-titles`.

COMPATIBILIDADE: o OpenCode atualiza com frequência e o schema do banco pode mudar; este
utilitário valida o schema e falha com mensagem clara em vez de reportar dados errados.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time

VERSION = "0.3.0"
TESTADO_COM = "opencode v2.0.20 (beta)"
BINARIO = ["opencode"]  # preenchido em main() (--bin / $OPENCODE_COMPACT_BIN); permite instâncias isoladas


def resolver_db(explicito: str | None = None) -> str:
    """--db > $OPENCODE_COMPACT_DB > `opencode debug paths` > padrões por SO."""
    if explicito:
        return os.path.expanduser(explicito)
    env = os.environ.get("OPENCODE_COMPACT_DB")
    if env:
        return os.path.expanduser(env)
    if shutil.which("opencode"):
        try:
            p = subprocess.run(["opencode", "debug", "paths"], capture_output=True,
                               text=True, timeout=20)
            for linha in (p.stdout or "").splitlines():
                m = re.match(r"\s*db\s+(\S+)\s*$", linha)
                if m:
                    return m.group(1)
        except Exception:
            pass
    if sys.platform == "darwin":
        return os.path.expanduser("~/Library/Application Support/opencode/opencode.db")
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA", os.path.expanduser("~/AppData/Local"))
        return os.path.join(base, "opencode", "opencode.db")
    return os.path.expanduser("~/.local/share/opencode/opencode.db")


def resolver_bin(explicito: str | None = None) -> str:
    """--bin > $OPENCODE_COMPACT_BIN > `opencode`.

    Necessário para instâncias isoladas (ex.: um wrapper `opencode-2` com XDG próprio,
    que fala com o serviço daquela instância). O banco correspondente vai em --db /
    $OPENCODE_COMPACT_DB — as duas pontas precisam apontar para a MESMA instância.
    """
    return explicito or os.environ.get("OPENCODE_COMPACT_BIN") or "opencode"


def parse_size(s: str) -> int:
    m = re.fullmatch(r"\s*(\d+(?:\.\d+)?)\s*([kKmM]?)\s*", s or "")
    if not m:
        raise argparse.ArgumentTypeError(f"tamanho inválido: {s!r} (use 600k, 1M, 900000)")
    val = float(m.group(1))
    mult = {"": 1, "k": 1_000, "m": 1_000_000}[m.group(2).lower()]
    return int(val * mult)


def human(n: int) -> str:
    if n >= 1_000_000:
        return f"{n/1_000_000:.2f}M"
    if n >= 1_000:
        return f"{n/1000:.0f}k"
    return str(n)


def idade(ms: int) -> str:
    if not ms:
        return "-"
    d = max(0, int(time.time() - ms / 1000))
    if d < 60:
        return f"{d}s"
    if d < 3600:
        return f"{d//60}min"
    return f"{d//3600}h{(d%3600)//60:02d}"


def load_sessions(db: str, idle_window: int, titulos: bool = True) -> list[dict]:
    """Contexto/tamanho + sinais de trabalho em voo, por sessão (somente metadados)."""
    con = sqlite3.connect("file:" + db + "?mode=ro", uri=True)
    try:  # validação de schema: falha clara em vez de resultado errado
        con.execute("select 1 from session_message limit 1").fetchone()
    except sqlite3.Error as e:
        raise SystemExit(
            f"schema inesperado em {db}: {e}\n"
            "O OpenCode pode ter mudado o schema nesta versão — verifique com:\n"
            f'  sqlite3 "{db}" ".tables"'
        ) from None

    meta = {}
    for sid, directory, title in con.execute("select id, directory, title from session_v2"):
        meta[sid] = {"directory": directory or "", "title": (title or "") if titulos else ""}
    filhos = {}
    for sid, parent in con.execute("select id, parent_id from session_v2 where parent_id is not null"):
        filhos.setdefault(parent, []).append(sid)
    pend = {}
    for tb in ("session_pending", "session_inbox"):
        try:
            for (sid, n) in con.execute(f"select session_id, count(*) from {tb} group by session_id"):
                pend[sid] = pend.get(sid, 0) + n
        except sqlite3.Error:
            pass

    last_ctx, last_msg, kids_last = {}, {}, {}
    sql = "select session_id, time_created, type, data from session_message order by time_created"
    for sid, t, typ, data in con.execute(sql):
        kids_last[sid] = t
        if typ != "assistant":
            continue
        try:
            d = json.loads(data)
        except Exception:
            continue
        tok = d.get("tokens") or {}
        model = (d.get("model") or {}).get("id") or d.get("modelID") or ""
        if tok and "deepseek" in model.lower():
            ctx = int(tok.get("input") or 0) + int((tok.get("cache") or {}).get("read") or 0)
            if ctx > 0:
                last_ctx[sid] = ctx
        last_msg[sid] = {"t": t, "finish": d.get("finish")}

    agora = int(time.time() * 1000)
    out = []
    for sid, ctx in last_ctx.items():
        info = last_msg.get(sid) or {}
        finish = info.get("finish")
        # subagente ativo = filho com atividade recente EM TERMOS ABSOLUTOS (idle_window)
        # e posterior ao último item do pai (evita marcar sessões antigas como "em voo").
        limite_abs = agora - idle_window * 1000
        limite_pai = info.get("t") or 0
        ativos = [k for k in filhos.get(sid, [])
                  if (kids_last.get(k) or 0) >= limite_abs and (kids_last.get(k) or 0) >= limite_pai]
        em_voo = []
        if finish == "tool-calls":
            em_voo.append("turno em andamento")
        if ativos:
            em_voo.append(f"{len(ativos)} subagente(s) ativo(s)")
        if pend.get(sid):
            em_voo.append(f"{pend[sid]} prompt(s) na fila")
        out.append({"session": sid, "ctx": ctx, "t": info.get("t") or 0, "finish": finish,
                    "kids": len(ativos), "pend": pend.get(sid, 0), "em_voo": em_voo,
                    "parada": bool(info.get("t")) and (agora - info["t"]) > 24 * 3600 * 1000,
                    "title": (meta.get(sid) or {}).get("title", ""),
                    "directory": (meta.get(sid) or {}).get("directory", "")})
    return sorted(out, key=lambda x: -x["t"])


def linha_status(sessions: list[dict], teto: int) -> str:
    """Uma linha compacta (ASCII) para barras/painéis (ex.: tclock). Sem linhas de log."""
    recentes = [s for s in sessions if not s.get("parada")]  # sessao parada nao interessa ao painel
    if not recentes:
        return "nenhuma sessao recente"
    maior = max(recentes, key=lambda s: s["ctx"])
    acima = [s for s in recentes if s["ctx"] >= teto]
    est = ""
    if maior["em_voo"]:
        est = f" em voo({len(maior['em_voo'])}x)"
    elif maior.get("parada"):
        est = " parada"
    return (f"maior {human(maior['ctx'])}{est} | "
            f"{len(acima)} acima de {human(teto)} | {len(recentes)} recentes")


def request_compaction(session: str, timeout: int = 60, binario: str = "opencode") -> tuple[bool, str]:
    """Pede a compactação via API local (roda no próximo ponto seguro, funde pedidos repetidos)."""
    cmd = [binario, "api", "POST", f"/api/session/{session}/compact", "-d", "{}"]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except FileNotFoundError:
        return False, f"binário `{binario}` não encontrado"
    except subprocess.TimeoutExpired:
        return False, f"timeout de {timeout}s ao chamar a API"
    out = (p.stdout or "").strip() or (p.stderr or "").strip()
    ok = p.returncode == 0 and "error" not in out.lower()
    return ok, out[:300]


# ----------------------------------------------------------------------------- TUI (curses)

def tui(args) -> int:
    import curses

    teto_ref = [args.above]
    estado = {"sel": 0, "armado": False, "ordem": "ctx", "msg": "", "carregado": 0.0,
              "auto": 20, "todas": False}
    sessions: list[dict] = []

    def recarregar():
        sessions.clear()
        try:
            todas = load_sessions(args.db, args.idle_window, not args.no_titles)
        except SystemExit as e:
            estado["msg"] = str(e).splitlines()[0]
            todas = []
        if estado["ordem"] == "ctx":
            todas.sort(key=lambda s: -s["ctx"])
        else:
            todas.sort(key=lambda s: -s["t"])
        sessions.extend(todas if estado["todas"] else [s for s in todas if not s.get("parada")])
        estado["sel"] = max(0, min(estado["sel"], len(sessions) - 1)) if sessions else 0
        estado["carregado"] = time.time()

    def cor(s):
        if s["em_voo"] and not estado["armado"]:
            return curses.color_pair(1) if curses.has_colors() else curses.A_DIM
        if s["ctx"] >= teto_ref[0]:
            return curses.color_pair(3) if curses.has_colors() else curses.A_BOLD
        return curses.color_pair(2) if curses.has_colors() else 0

    def desenhar(stdscr):
        h, w = stdscr.getmaxyx()
        stdscr.erase()
        titulo = (f" Compactação por teto — OpenCode v{VERSION}   teto={human(teto_ref[0])}   "
                  f"[{'ARMADO' if estado['armado'] else 'SIMULAÇÃO'}]   {estado['ordem']}   "
                  f"{'todas' if estado['todas'] else 'recentes(24h)'}")
        try:
            stdscr.addnstr(0, 0, titulo.ljust(w - 1)[:w - 1], w - 1, curses.A_REVERSE)
            stdscr.addnstr(1, 0, f"{'':>2} {'sessão':<26} {'contexto':>9} {'estado':<28} {'visto':>7}  título"[:w - 1],
                           w - 1, curses.A_BOLD)
        except curses.error:
            pass
        visiveis = max(1, h - 5)
        inicio = max(0, min(estado["sel"] - visiveis // 3, max(0, len(sessions) - visiveis)))
        for i, s in enumerate(sessions[inicio:inicio + visiveis], start=inicio):
            est = ", ".join(s["em_voo"]) if s["em_voo"] else ("parada" if s.get("parada") else "livre")
            linha = (f"{'→' if i == estado['sel'] else ' '} {s['session'][:26]:<26} {human(s['ctx']):>9} "
                     f"{est[:28]:<28} {idade(s['t']):>7}  {(s['title'] or '')[:max(0, w - 80)]}")
            attr = curses.A_REVERSE if i == estado["sel"] else cor(s)
            try:
                stdscr.addnstr(2 + (i - inicio), 0, linha[:w - 1], w - 1, attr)
            except curses.error:
                pass
        if not sessions:
            try:
                stdscr.addnstr(3, 2, "nenhuma sessão recente (use 't' para mostrar as paradas)", w - 4)
            except curses.error:
                pass
        dica = ("↑/↓ mover · r atualizar · a armar · +/- teto · o ordenar · t todas · "
                "c compactar · C forçar · q sair")
        try:
            stdscr.addnstr(h - 2, 0, dica[:w - 1], w - 1, curses.A_DIM if hasattr(curses, "A_DIM") else 0)
            stdscr.addnstr(h - 1, 0, (estado["msg"] or " ")[:w - 1], w - 1)
        except curses.error:
            pass
        stdscr.refresh()

    def confirmar(stdscr, texto: str, palavra: str | None = None) -> bool:
        h, w = stdscr.getmaxyx()
        curses.echo()
        curses.curs_set(1)
        try:
            stdscr.addnstr(h - 1, 0, " " * (w - 1), w - 1)
            stdscr.addnstr(h - 1, 0, texto[:w - 2], w - 2, curses.A_BOLD)
            stdscr.move(h - 1, min(len(texto) + 1, w - 2))
            resp = stdscr.getstr().decode(errors="ignore").strip()
        except curses.error:
            resp = ""
        finally:
            curses.noecho()
            curses.curs_set(0)
        return resp.lower() == palavra if palavra else resp.lower() in ("s", "sim", "y", "yes")

    def agir(stdscr, forcar: bool):
        if not sessions:
            return
        s = sessions[estado["sel"]]
        if s["em_voo"] and not forcar:
            estado["msg"] = f"RECUSADO: {', '.join(s['em_voo'])} — use C para forçar (lossy!)"
            return
        if not estado["armado"] and not forcar:
            estado["msg"] = (f"SIMULAÇÃO: pediria compactar {s['session'][:20]} ({human(s['ctx'])}). "
                             f"Pressione 'a' para armar.")
            return
        risco = " ATENÇÃO: trabalho em voo!" if s["em_voo"] else ""
        if forcar and s["em_voo"]:
            ok = confirmar(stdscr, f'Compactar {s["session"][:20]} ({human(s["ctx"])})?{risco} '
                                   f'digite "force":', palavra="force")
        else:
            ok = confirmar(stdscr, f'Compactar {s["session"][:20]} ({human(s["ctx"])})?{risco} [s/N]')
        if not ok:
            estado["msg"] = "cancelado"
            return
        ok, msg = request_compaction(s["session"], binario=BINARIO[0])
        estado["msg"] = f"{'pedido aceito' if ok else 'FALHOU'}: {msg[:80]}"
        recarregar()

    def loop(stdscr):
        curses.curs_set(0)
        stdscr.keypad(True)
        if curses.has_colors():
            try:
                curses.start_color()
                curses.use_default_colors()
                curses.init_pair(1, curses.COLOR_RED, -1)
                curses.init_pair(2, curses.COLOR_WHITE, -1)
                curses.init_pair(3, curses.COLOR_GREEN, -1)
            except curses.error:
                pass
        recarregar()
        while True:
            desenhar(stdscr)
            stdscr.timeout(1000)
            ch = stdscr.getch()
            if ch == -1:
                if time.time() - estado["carregado"] > estado["auto"]:
                    recarregar()
                continue
            if ch in (ord("q"), 27):
                return 0
            if ch in (curses.KEY_UP, ord("k")):
                estado["sel"] = max(0, estado["sel"] - 1)
            elif ch in (curses.KEY_DOWN, ord("j")):
                estado["sel"] = min(max(0, len(sessions) - 1), estado["sel"] + 1)
            elif ch == ord("r"):
                recarregar()
                estado["msg"] = "atualizado"
            elif ch == ord("a"):
                estado["armado"] = not estado["armado"]
                estado["msg"] = ("ARMADO: c vai pedir compactação de verdade"
                                 if estado["armado"] else "SIMULAÇÃO: nada é enviado")
            elif ch in (ord("+"), ord("=")):
                teto_ref[0] = min(1_000_000, teto_ref[0] + 100_000)
            elif ch == ord("-"):
                teto_ref[0] = max(0, teto_ref[0] - 100_000)
            elif ch == ord("o"):
                estado["ordem"] = "recencia" if estado["ordem"] == "ctx" else "ctx"
                recarregar()
            elif ch == ord("t"):
                estado["todas"] = not estado["todas"]
                recarregar()
            elif ch == ord("c"):
                agir(stdscr, forcar=False)
            elif ch == ord("C"):
                agir(stdscr, forcar=True)

    return curses.wrapper(loop)


# ----------------------------------------------------------------------------- CLI

def main(argv=None) -> int:
    ap = argparse.ArgumentParser(
        prog="opencode-compact-if-big",
        description="Pede compactação quando o contexto passar do teto (com travas).")
    ap.add_argument("--above", type=parse_size, default=600_000, help="teto de contexto (padrão: 600k)")
    ap.add_argument("--apply", action="store_true", help="age de fato (padrão: dry-run)")
    ap.add_argument("--force", action="store_true", help="age mesmo com trabalho em voo (perigoso)")
    ap.add_argument("--session", help="sessão específica (padrão: a mais recente acima do teto)")
    ap.add_argument("--all", action="store_true", help="considera todas as sessões acima do teto")
    ap.add_argument("--list", action="store_true", help="apenas lista as sessões e tamanhos")
    ap.add_argument("--status", action="store_true",
                    help="uma linha compacta (para painéis/barras, ex.: tclock); não age")
    ap.add_argument("--tui", action="store_true", help="interface interativa (curses)")
    ap.add_argument("--no-titles", action="store_true", help="não exibe títulos de sessão (privacidade)")
    ap.add_argument("--idle-window", type=int, default=600,
                    help="janela (s) para considerar subagente ativo (padrão: 600)")
    ap.add_argument("--limit", type=int, default=10, help="linhas no relatório (padrão: 10)")
    ap.add_argument("--db", default=None, help="caminho do opencode.db (padrão: autodetectado)")
    ap.add_argument("--bin", default=None,
                    help="binário que fala com a API da instância (padrão: opencode; "
                         "use opencode-2 para instâncias isoladas)")
    ap.add_argument("-V", "--version", action="version",
                    version=f"opencode-compact-if-big {VERSION} (testado com {TESTADO_COM})")
    args = ap.parse_args(argv)
    args.db = resolver_db(args.db)
    BINARIO[0] = resolver_bin(args.bin)

    if args.status:
        if not os.path.exists(args.db):
            print(f"banco ausente: {args.db}")
            return 2
        try:
            print(linha_status(load_sessions(args.db, args.idle_window, False), args.above))
        except SystemExit as e:
            print(str(e).splitlines()[0])
            return 2
        return 0

    if args.tui:
        if not os.path.exists(args.db):
            print(f"M=compactIfBig, E=\"banco ausente: {args.db}\", status=error", file=sys.stderr)
            return 2
        return tui(args)

    print(f'M=compactIfBig, I="iniciando", teto={human(args.above)}, modo='
          f'{"apply" if args.apply else "dry-run"}, bin={BINARIO[0]}, status=init')

    if not os.path.exists(args.db):
        print(f'M=compactIfBig, E="banco ausente: {args.db}" (use --db ou $OPENCODE_COMPACT_DB), '
              f'status=error', file=sys.stderr)
        return 2

    sessions = load_sessions(args.db, args.idle_window, not args.no_titles)
    if args.session:
        sessions = [s for s in sessions if s["session"] == args.session] or \
                   [{"session": args.session, "ctx": 0, "title": "(desconhecida)", "directory": "",
                     "t": 0, "finish": None, "kids": 0, "pend": 0, "em_voo": [], "parada": False}]

    if args.list or not sessions:
        for s in sessions[:args.limit]:
            quando = time.strftime("%d/%m %H:%M", time.localtime(s["t"] / 1000)) if s["t"] else "-"
            est = ("EM VOO: " + "; ".join(s["em_voo"])) if s["em_voo"] else \
                  ("parada" if s.get("parada") else "livre")
            print(f'  {s["session"][:26]:<28} {human(s["ctx"]):>8}  {quando}  {est[:60]}  {(s["title"] or "")[:28]}')
        if not sessions:
            print("  (nenhuma sessão com tokens registrados)")
        return 0

    alvos = [s for s in sessions if s["ctx"] >= args.above]
    if not alvos:
        print(f'M=compactIfBig, I="nada a fazer", maior={human(sessions[0]["ctx"])}, status=complete')
        return 0
    if not args.all:
        alvos = [alvos[0]]

    rc = 0
    for s in alvos:
        print(f'  alvo {s["session"][:26]} contexto={human(s["ctx"])} '
              f'titulo="{(s["title"] or "")[:40]}" cwd={s["directory"]}')
        if s["em_voo"] and not args.force:
            print(f'  -> RECUSADO: trabalho em voo ({"; ".join(s["em_voo"])}). '
                  f'Espere o turno/subagentes terminarem ou use --force (lossy e irreversível).')
            rc = 3
            continue
        if s["em_voo"] and args.force:
            print(f'  -> ATENÇÃO: --force com trabalho em voo ({"; ".join(s["em_voo"])})')
        if not args.apply:
            print(f'  -> [dry-run] pediria: opencode api POST /api/session/{s["session"]}/compact')
            continue
        ok, msg = request_compaction(s["session"], binario=BINARIO[0])
        print(f'  -> {"pedido aceito" if ok else "FALHOU"}: {msg}')
        if not ok:
            rc = 1
    print(f'M=compactIfBig, I="concluido", alvos={len(alvos)}, aplicado={args.apply}, status=complete')
    return rc


if __name__ == "__main__":
    sys.exit(main())
