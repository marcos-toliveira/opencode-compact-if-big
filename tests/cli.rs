//! Testes de integração do binário `opencode-compact-if-big`.
//!
//! Constroem um banco SQLite temporário com o schema mínimo do OpenCode e
//! exercitam a CLI ponta a ponta via `std::process::Command`. Quando o Python
//! original está disponível e `OCIB_PARITY_PY` aponta para ele, alguns testes
//! comparam a saída **byte a byte** com a do Python.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

const SCHEMA: &str = "\
create table session_v2 (id text primary key, directory text, title text, parent_id text);
create table session_message (
  id integer primary key autoincrement, session_id text, time_created integer,
  type text, data text);
create table session_pending (
  session_id text, type text, delivery text, time_created integer);
create table session_inbox (
  session_id text, type text, payload text, delivery text, time_created integer);
";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Banco SQLite temporário construído com `sqlite3` (mesmo caminho da produção).
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(statements: &[String]) -> Self {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".ocib-test-{}-{sequence}.db", std::process::id()));
            if path.exists() {
                continue;
            }
            let script = format!("{}; {}", SCHEMA.replace('\n', " "), statements.join("; "));
            let status = Command::new("sqlite3")
                .arg(&path)
                .arg(&script)
                .status()
                .expect("sqlite3 deve estar disponível para os testes");
            assert!(status.success(), "não foi possível criar o fixture");
            return Self { path };
        }
    }

    fn path(&self) -> &str {
        self.path.to_str().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_opencode-compact-if-big")
}

fn run(arguments: &[&str]) -> Output {
    Command::new(binary()).args(arguments).output().unwrap()
}

fn json_string(text: &str) -> String {
    // O JSON do `data` é montado à mão e escapado para o shell do sqlite3.
    text.replace('\'', "''")
}

fn session(id: &str, title: &str) -> String {
    format!("insert into session_v2 values ('{id}', '/home/user/proj', '{title}', null)")
}

fn assistant(id: &str, t: i64, cache_read: i64, finish: &str) -> String {
    let data = format!(
        "{{\"tokens\": {{\"output\": 100, \"input\": 10, \"cache\": {{\"read\": {cache_read}}}}}, \
         \"model\": {{\"id\": \"deepseek-v4.1-flash\"}}, \"finish\": \"{finish}\"}}"
    );
    format!(
        "insert into session_message (session_id, time_created, type, data) \
         values ('{id}', {t}, 'assistant', '{}')",
        json_string(&data)
    )
}

/// Fixture padrão: livre/800k, turno/700k, fila/750k, parada/650k, abaixo/10k.
fn fixture_padrao() -> Fixture {
    let agora = now_ms();
    Fixture::new(&[
        session("ses_livre00000001", "Sessao Livre"),
        assistant("ses_livre00000001", agora - 40 * 60_000, 800_000, "stop"),
        session("ses_turno00000002", "Sessao Turno"),
        assistant(
            "ses_turno00000002",
            agora - 40 * 60_000,
            700_000,
            "tool-calls",
        ),
        session("ses_fila000000003", "Sessao Fila"),
        assistant("ses_fila000000003", agora - 40 * 60_000, 750_000, "stop"),
        format!(
            "insert into session_pending values ('ses_fila000000003', 'prompt', 'queued', {agora})"
        ),
        session("ses_parada0000004", "Sessao Parada"),
        assistant("ses_parada0000004", agora - 48 * 3_600_000, 650_000, "stop"),
        session("ses_abaixoooo0005", "Sessao Abaixo"),
        assistant("ses_abaixoooo0005", agora - 40 * 60_000, 10_000, "stop"),
    ])
}

#[test]
fn version_imprime_string_canonica() {
    let output = run(&["-V"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "opencode-compact-if-big 0.6.0 (testado com opencode v2.0.20 (beta))"
    );
}

#[test]
fn list_mostra_sessoes_estado_e_contexto() {
    let fixture = fixture_padrao();
    let output = run(&["--db", fixture.path(), "--list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("M=compactIfBig, I=\"iniciando\""),
        "{stdout}"
    );
    assert!(stdout.contains("800k"), "{stdout}");
    assert!(stdout.contains("EM VOO: turno em andamento"), "{stdout}");
    assert!(stdout.contains("EM VOO: 1 prompt(s) na fila"), "{stdout}");
    assert!(stdout.contains("parada"), "{stdout}");
    assert!(stdout.contains("Sessao Livre"), "{stdout}");
}

#[test]
fn status_uma_linha_compacta() {
    let fixture = fixture_padrao();
    let output = run(&["--db", fixture.path(), "--status", "--sessions", "3"]);
    assert!(output.status.success());
    let linhas: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(linhas[0], "maior 800k | 3 acima de 600k | 4 recentes");
    assert_eq!(linhas.len(), 4, "cabeçalho + 3 sessões: {linhas:?}");
    assert!(
        linhas[1].contains("800k") && linhas[1].contains("livre"),
        "{linhas:?}"
    );
    assert!(linhas.iter().any(|l| l.contains("fila1")), "{linhas:?}");
}

#[test]
fn dry_run_nao_chama_a_api() {
    let fixture = fixture_padrao();
    // `--bin` inválido garante que qualquer chamada real falharia; o dry-run não chama.
    let output = run(&[
        "--db",
        fixture.path(),
        "--above",
        "600k",
        "--bin",
        "/bin/nao-existe-opencode",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[dry-run] pediria"), "{stdout}");
    assert!(
        stdout.contains("api POST /api/session/ses_livre00000001/compact"),
        "{stdout}"
    );
    assert!(stdout.contains("alvos=1"), "{stdout}");
    assert!(
        !stdout.contains("FALHOU"),
        "a API não pode ser chamada no dry-run: {stdout}"
    );
    assert!(!stdout.contains("não encontrado"), "{stdout}");
}

#[test]
fn above_alto_nao_faz_nada() {
    let fixture = fixture_padrao();
    let output = run(&["--db", fixture.path(), "--above", "900k", "--apply"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("I=\"nada a fazer\""), "{stdout}");
    assert!(stdout.contains("maior=800k"), "{stdout}");
}

#[test]
fn above_com_sufixo_k_e_m_equivalentes() {
    let fixture = fixture_padrao();
    let k = run(&["--db", fixture.path(), "--above", "600k", "--list"]);
    let m = run(&["--db", fixture.path(), "--above", "0.6M", "--list"]);
    assert_eq!(k.stdout, m.stdout, "600k e 0.6M devem ser o mesmo teto");
}

#[test]
fn apply_recusa_quando_ha_trabalho_em_voo() {
    let fixture = fixture_padrao();
    let output = run(&[
        "--db",
        fixture.path(),
        "--above",
        "600k",
        "--session",
        "ses_turno00000002",
        "--apply",
        "--bin",
        "/bin/nao-existe-opencode",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("RECUSADO: trabalho em voo"), "{stdout}");
    assert!(
        !stdout.contains("FALHOU"),
        "não pode tentar a API com trabalho em voo: {stdout}"
    );
}

#[test]
fn apply_com_fila_tambem_recusa() {
    let fixture = fixture_padrao();
    let output = run(&[
        "--db",
        fixture.path(),
        "--above",
        "600k",
        "--session",
        "ses_fila000000003",
        "--apply",
        "--bin",
        "/bin/nao-existe-opencode",
    ]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("1 prompt(s) na fila"), "{stdout}");
}

#[test]
fn banco_ausente_retorna_2() {
    let output = run(&["--db", "/caminho/que/nao/existe.db"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("banco ausente"), "{stderr}");
    assert!(stderr.contains("status=error"), "{stderr}");
}

#[test]
fn tui_com_banco_ausente_retorna_2() {
    let output = run(&["--db", "/caminho/que/nao/existe.db", "--tui"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("banco ausente"), "{stderr}");
}

#[test]
fn no_titles_esconde_titulos_do_list() {
    let fixture = fixture_padrao();
    let output = run(&["--db", fixture.path(), "--list", "--no-titles"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("Sessao Livre"), "{stdout}");
    assert!(stdout.contains("ses_livre00000001"), "{stdout}");
}

#[test]
fn once_dry_run_lista_alvos_elegiveis() {
    let fixture = fixture_padrao();
    let output = run(&[
        "--db",
        fixture.path(),
        "--above",
        "600k",
        "--once",
        "--ocioso",
        "15",
        "--bin",
        "/bin/nao-existe-opencode",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("teto=600k"), "{stdout}");
    assert!(stdout.contains("elegiveis=2"), "{stdout}");
    assert!(stdout.contains("modo=aviso"), "{stdout}");
    assert!(stdout.contains("[aviso] compactaria agora"), "{stdout}");
    assert!(!stdout.contains("FALHOU"), "{stdout}");
}

#[test]
fn once_apply_chama_api_e_falha_com_binario_ausente() {
    let fixture = fixture_padrao();
    let output = run(&[
        "--db",
        fixture.path(),
        "--above",
        "600k",
        "--session",
        "ses_livre00000001",
        "--all",
        "--once",
        "--ocioso",
        "15",
        "--apply",
        "--bin",
        "/bin/nao-existe-opencode",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("modo=APPLY"), "{stdout}");
    assert!(stdout.contains("FALHOU"), "{stdout}");
    assert!(stdout.contains("não encontrado"), "{stdout}");
}

/// Paridade byte a byte com o Python, quando `OCIB_PARITY_PY` aponta para ele.
#[test]
fn paridade_com_python_quando_disponivel() {
    let Ok(python) = std::env::var("OCIB_PARITY_PY") else {
        eprintln!("OCIB_PARITY_PY não definido — pulando teste de paridade");
        return;
    };
    let fixture = fixture_padrao();
    let casos: &[&[&str]] = &[
        &["--list"],
        &["--status"],
        &["--status", "--sessions", "4"],
        &["--above", "600k"],
        &["--above", "600k", "--list"],
        &["--above", "900k"],
        &["--above", "600k", "--no-titles", "--list"],
    ];
    for caso in casos {
        let mut rust = vec!["--db", fixture.path()];
        rust.extend_from_slice(caso);
        let esperado = Command::new(&python)
            .args(&rust)
            .output()
            .expect("Python de paridade deve rodar");
        let obtido = run(&rust);
        assert_eq!(
            obtido.stdout, esperado.stdout,
            "stdout divergente para {caso:?}"
        );
        assert_eq!(
            obtido.status.code(),
            esperado.status.code(),
            "código de saída divergente para {caso:?}"
        );
    }
}
