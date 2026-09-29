#!/usr/bin/env python3
"""Testes do opencode-compact-if-big — travas, dry-run e privacidade.

Constrói um banco SQLite temporário com o schema mínimo e verifica as decisões do
utilitário sem depender de um OpenCode em execução.

    python3 -m unittest discover -s tests -v
"""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
import sqlite3
import tempfile
import time
import unittest

RAIZ = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CAMINHO = os.path.join(RAIZ, "opencode-compact-if-big")

SCHEMA = """
create table session_v2 (id text primary key, directory text, title text, parent_id text);
create table session_message (
  id integer primary key autoincrement, session_id text, time_created integer,
  type text, data text);
create table session_pending (
  session_id text, type text, delivery text, time_created integer);
create table session_inbox (
  session_id text, type text, payload text, delivery text, time_created integer);
"""


def carregar_modulo():
    loader = importlib.machinery.SourceFileLoader("ocib", CAMINHO)
    spec = importlib.util.spec_from_loader("ocib", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


OCIB = carregar_modulo()
AGORA = int(time.time() * 1000)


def criar_db(linhas):
    fd, caminho = tempfile.mkstemp(suffix=".db")
    os.close(fd)
    con = sqlite3.connect(caminho)
    con.executescript(SCHEMA)
    for tabela, *valores in linhas:
        if tabela == "session_v2":
            con.execute("insert into session_v2 (id,directory,title,parent_id) values (?,?,?,?)", valores)
        elif tabela == "session_message":
            con.execute("insert into session_message (session_id,time_created,type,data) values (?,?,?,?)",
                        valores)
        elif tabela == "session_pending":
            con.execute("insert into session_pending (session_id,type,delivery,time_created) values (?,?,?,?)",
                        valores)
    con.commit()
    con.close()
    return caminho


def assistant(sid, t, cache=200_000, finish="stop", out=100):
    data = {"tokens": {"output": out, "input": 10, "cache": {"read": cache}},
            "model": {"id": "deepseek-v4.1-flash"}, "finish": finish}
    return ("session_message", sid, t, "assistant", json.dumps(data))


def sessao(sid, titulo="titulo secreto do cliente", cwd="/home/user/projeto-confidencial", parent=None):
    return ("session_v2", sid, cwd, titulo, parent)


class TestTravas(unittest.TestCase):
    def decidir(self, linhas, sid, idle_window=600):
        caminho = criar_db(linhas)
        try:
            sessoes = OCIB.load_sessions(caminho, idle_window)
            alvo = [s for s in sessoes if s["session"] == sid]
            self.assertTrue(alvo, "sessão não encontrada")
            return alvo[0]
        finally:
            os.unlink(caminho)

    def test_sessao_livre(self):
        s = self.decidir([sessao("ses_a"), assistant("ses_a", AGORA)], "ses_a")
        self.assertEqual(s["em_voo"], [])
        self.assertGreaterEqual(s["ctx"], 200_000)

    def test_turno_em_andamento(self):
        s = self.decidir([sessao("ses_a"), assistant("ses_a", AGORA, finish="tool-calls")], "ses_a")
        self.assertTrue(any("turno em andamento" in m for m in s["em_voo"]))

    def test_subagente_ativo(self):
        linhas = [sessao("ses_pai"), assistant("ses_pai", AGORA - 60_000),
                  sessao("ses_filho", cwd="/tmp", parent="ses_pai"),
                  assistant("ses_filho", AGORA - 5_000)]
        s = self.decidir(linhas, "ses_pai")
        self.assertTrue(any("subagente" in m for m in s["em_voo"]), s["em_voo"])

    def test_subagente_antigo_nao_marca_em_voo(self):
        """Regressão: a janela relativa ao pai marcava sessões antigas como 'em voo' para sempre."""
        linhas = [sessao("ses_pai"), assistant("ses_pai", AGORA),
                  sessao("ses_filho", cwd="/tmp", parent="ses_pai"),
                  assistant("ses_filho", AGORA - 3 * 3600 * 1000)]
        s = self.decidir(linhas, "ses_pai")
        self.assertEqual(s["em_voo"], [], s["em_voo"])

    def test_prompt_na_fila(self):
        linhas = [sessao("ses_a"), assistant("ses_a", AGORA),
                  ("session_pending", "ses_a", "prompt", "queued", AGORA)]
        s = self.decidir(linhas, "ses_a")
        self.assertTrue(any("fila" in m for m in s["em_voo"]), s["em_voo"])

    def test_parada_marcada(self):
        s = self.decidir([sessao("ses_a"), assistant("ses_a", AGORA - 48 * 3600 * 1000)], "ses_a")
        self.assertTrue(s["parada"])

    def test_no_titles_esconde_titulo(self):
        caminho = criar_db([sessao("ses_a"), assistant("ses_a", AGORA)])
        try:
            com = OCIB.load_sessions(caminho, 600, True)[0]
            sem = OCIB.load_sessions(caminho, 600, False)[0]
            self.assertTrue(com["title"])
            self.assertEqual(sem["title"], "")
        finally:
            os.unlink(caminho)


class TestCLI(unittest.TestCase):
    def rodar(self, argv):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = OCIB.main(argv)
        return rc, buf.getvalue()

    def test_dry_run_nao_chama_api(self):
        caminho = criar_db([sessao("ses_a"), assistant("ses_a", AGORA, cache=800_000)])
        chamado = {"n": 0}
        original = OCIB.request_compaction
        OCIB.request_compaction = lambda *a, **k: (chamado.__setitem__("n", chamado["n"] + 1), (True, ""))[1]
        try:
            rc, saida = self.rodar(["--db", caminho, "--above", "600k"])
            self.assertEqual(chamado["n"], 0, "dry-run não pode chamar a API")
            self.assertIn("dry-run", saida)
            self.assertEqual(rc, 0)
        finally:
            OCIB.request_compaction = original
            os.unlink(caminho)

    def test_apply_recusa_com_trabalho_em_voo(self):
        caminho = criar_db([sessao("ses_a"), assistant("ses_a", AGORA, cache=800_000,
                                                       finish="tool-calls")])
        chamado = {"n": 0}
        original = OCIB.request_compaction
        OCIB.request_compaction = lambda *a, **k: (chamado.__setitem__("n", chamado["n"] + 1), (True, ""))[1]
        try:
            rc, saida = self.rodar(["--db", caminho, "--above", "600k", "--apply"])
            self.assertEqual(chamado["n"], 0, "com trabalho em voo não pode chamar a API")
            self.assertIn("RECUSADO", saida)
            self.assertEqual(rc, 3)
        finally:
            OCIB.request_compaction = original
            os.unlink(caminho)

    def test_apply_chama_api_quando_livre(self):
        caminho = criar_db([sessao("ses_a"), assistant("ses_a", AGORA, cache=800_000)])
        chamado = {"n": 0}
        original = OCIB.request_compaction
        OCIB.request_compaction = lambda *a, **k: (chamado.__setitem__("n", chamado["n"] + 1), (True, "ok"))[1]
        try:
            rc, _ = self.rodar(["--db", caminho, "--above", "600k", "--apply"])
            self.assertEqual(chamado["n"], 1)
            self.assertEqual(rc, 0)
        finally:
            OCIB.request_compaction = original
            os.unlink(caminho)

    def test_abaixo_do_teto_nao_faz_nada(self):
        caminho = criar_db([sessao("ses_a"), assistant("ses_a", AGORA, cache=10_000)])
        try:
            rc, saida = self.rodar(["--db", caminho, "--above", "600k", "--apply"])
            self.assertIn("nada a fazer", saida)
            self.assertEqual(rc, 0)
        finally:
            os.unlink(caminho)

    def test_banco_ausente(self):
        rc, _ = self.rodar(["--db", "/caminho/que/nao/existe.db"])
        self.assertEqual(rc, 2)


class TestResolucaoDeDb(unittest.TestCase):
    def test_explicito_vence(self):
        self.assertEqual(OCIB.resolver_db("/tmp/x.db"), "/tmp/x.db")

    def test_env(self):
        os.environ["OPENCODE_COMPACT_DB"] = "/tmp/env.db"
        try:
            self.assertEqual(OCIB.resolver_db(None), "/tmp/env.db")
        finally:
            del os.environ["OPENCODE_COMPACT_DB"]

    def test_tamanhos(self):
        self.assertEqual(OCIB.parse_size("600k"), 600_000)
        self.assertEqual(OCIB.parse_size("1M"), 1_000_000)
        self.assertEqual(OCIB.parse_size("900000"), 900_000)
        self.assertEqual(OCIB.human(600_000), "600k")
        self.assertEqual(OCIB.human(0), "0")


class TestInstanciaIsolada(unittest.TestCase):
    """Instâncias isoladas (ex.: opencode-2) exigem binário + banco da MESMA instância."""

    def test_resolver_bin_precedencia(self):
        self.assertEqual(OCIB.resolver_bin("opencode-2"), "opencode-2")
        os.environ["OPENCODE_COMPACT_BIN"] = "opencode-9"
        try:
            self.assertEqual(OCIB.resolver_bin(None), "opencode-9")
            self.assertEqual(OCIB.resolver_bin("opencode-3"), "opencode-3")
        finally:
            del os.environ["OPENCODE_COMPACT_BIN"]
        self.assertEqual(OCIB.resolver_bin(None), "opencode")

    def test_request_compaction_usa_o_binario(self):
        capturado = {}

        class Falso:
            returncode = 0
            stdout = "aceito"
            stderr = ""

        original = OCIB.subprocess.run

        def fake(cmd, **kwargs):
            capturado["cmd"] = cmd
            return Falso()

        OCIB.subprocess.run = fake
        try:
            ok, _ = OCIB.request_compaction("ses_alvo", binario="opencode-2")
            self.assertTrue(ok)
            self.assertEqual(capturado["cmd"][0], "opencode-2",
                             "a chamada deve sair pelo binário da instância")
            self.assertIn("/api/session/ses_alvo/compact", capturado["cmd"])
        finally:
            OCIB.subprocess.run = original


if __name__ == "__main__":
    unittest.main()
