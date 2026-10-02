//! opencode-compact-if-big — pede compactação quando o contexto passa do teto.
//!
//! Porte em Rust (edição 2024, **zero dependências** — só a std) do utilitário
//! Python homônimo. Mede o contexto da última requisição (`tokens.input +
//! tokens.cache.read` do último assistant DeepSeek) e pede a compactação no
//! momento escolhido pelo operador, recusando agir quando há trabalho em voo.
//!
//! O banco SQLite é lido **somente leitura** via `sqlite3 -readonly -json` e
//! **apenas metadados** (`session_v2`, `session_message`, `session_pending`,
//! `session_inbox`) — nunca credenciais nem conteúdo de conversa. A API de
//! compactação é chamada por subprocesso: `<bin> api POST /api/session/<id>/compact -d {}`.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = "0.6.0";
const TESTADO_COM: &str = "opencode v2.0.20 (beta)";
const DEFAULT_BIN: &str = "opencode";
const BACKOFF_MIN: f64 = 60.0;
const MAX_JSON_DEPTH: usize = 128;

/// `println!` tolerante a pipe fechado (ex.: `| head`): sai com 0 em vez de
/// entrar em pânico ao escrever num stdout cujo leitor já encerrou.
macro_rules! println {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        let mut saida = std::io::stdout().lock();
        if std::io::Write::write_all(&mut saida, format!("{line}\n").as_bytes()).is_err() {
            std::process::exit(0);
        }
    }};
}

// =========================================================================== JSON

#[derive(Debug, Clone, PartialEq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(values) => values.get(key),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    input: &'a [u8],
    cursor: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            cursor: 0,
            depth: 0,
        }
    }

    fn parse(mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.cursor != self.input.len() {
            return Err(self.error("conteúdo inesperado após o valor JSON"));
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        if matches!(self.peek(), Some(b'{' | b'[')) {
            if self.depth >= MAX_JSON_DEPTH {
                return Err(self.error("JSON excedeu o limite de aninhamento"));
            }
            self.depth += 1;
            let value = if self.peek() == Some(b'{') {
                self.parse_object()
            } else {
                self.parse_array()
            };
            self.depth -= 1;
            return value;
        }
        match self.peek() {
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => {
                self.consume_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.consume_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'n') => {
                self.consume_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("valor JSON inválido")),
            None => Err(self.error("fim inesperado do arquivo JSON")),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.expect(b'{')?;
        self.skip_whitespace();
        let mut values = BTreeMap::new();
        if self.consume_if(b'}') {
            return Ok(JsonValue::Object(values));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("a chave do objeto deve ser uma string"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.parse_value()?;
            values.insert(key, value);
            self.skip_whitespace();
            if self.consume_if(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Object(values))
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.expect(b'[')?;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_if(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume_if(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Array(values))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut value = String::new();
        let mut segment_start = self.cursor;
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.push_utf8_segment(&mut value, segment_start, self.cursor)?;
                    self.cursor += 1;
                    return Ok(value);
                }
                b'\\' => {
                    self.push_utf8_segment(&mut value, segment_start, self.cursor)?;
                    self.cursor += 1;
                    let escaped = self
                        .peek()
                        .ok_or_else(|| self.error("sequência de escape incompleta"))?;
                    self.cursor += 1;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{0008}'),
                        b'f' => value.push('\u{000c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => self.push_unicode_escape(&mut value)?,
                        _ => return Err(self.error("sequência de escape inválida")),
                    }
                    segment_start = self.cursor;
                }
                0..=0x1f => return Err(self.error("caractere de controle dentro de string")),
                _ => self.cursor += 1,
            }
        }
        Err(self.error("string JSON não foi fechada"))
    }

    fn push_utf8_segment(
        &self,
        target: &mut String,
        start: usize,
        end: usize,
    ) -> Result<(), String> {
        let segment = std::str::from_utf8(&self.input[start..end])
            .map_err(|_| self.error("string não contém UTF-8 válido"))?;
        target.push_str(segment);
        Ok(())
    }

    fn push_unicode_escape(&mut self, target: &mut String) -> Result<(), String> {
        let first = self.parse_hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            if self.peek() != Some(b'\\') || self.input.get(self.cursor + 1) != Some(&b'u') {
                return Err(self.error("par substituto Unicode incompleto"));
            }
            self.cursor += 2;
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(self.error("par substituto Unicode inválido"));
            }
            0x10000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
        } else if (0xdc00..=0xdfff).contains(&first) {
            return Err(self.error("substituto Unicode isolado"));
        } else {
            first as u32
        };
        let character =
            char::from_u32(scalar).ok_or_else(|| self.error("código Unicode inválido"))?;
        target.push(character);
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u16, String> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self
                .peek()
                .ok_or_else(|| self.error("escape Unicode incompleto"))?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("escape Unicode inválido")),
            };
            value = (value << 4) | u16::from(digit);
            self.cursor += 1;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.cursor;
        self.consume_if(b'-');
        match self.peek() {
            Some(b'0') => {
                self.cursor += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("número JSON não pode ter zero à esquerda"));
                }
            }
            Some(b'1'..=b'9') => {
                self.cursor += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.cursor += 1;
                }
            }
            _ => return Err(self.error("parte inteira do número inválida")),
        }
        if self.consume_if(b'.') {
            let fraction_start = self.cursor;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == fraction_start {
                return Err(self.error("parte decimal do número inválida"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.cursor += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.cursor += 1;
            }
            let exponent_start = self.cursor;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == exponent_start {
                return Err(self.error("expoente do número inválido"));
            }
        }
        let text = std::str::from_utf8(&self.input[start..self.cursor])
            .map_err(|_| self.error("número JSON inválido"))?;
        let number = text
            .parse::<f64>()
            .map_err(|_| self.error("número JSON inválido"))?;
        if !number.is_finite() {
            return Err(self.error("número fora do intervalo suportado"));
        }
        Ok(JsonValue::Number(number))
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), String> {
        let end = self.cursor + literal.len();
        if self.input.get(self.cursor..end) == Some(literal) {
            self.cursor = end;
            Ok(())
        } else {
            Err(self.error("literal JSON inválido"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.cursor += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        if self.consume_if(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("esperado '{}'", expected as char)))
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.cursor).copied()
    }

    fn error(&self, message: &str) -> String {
        let consumed = String::from_utf8_lossy(&self.input[..self.cursor.min(self.input.len())]);
        let line = consumed.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = consumed
            .rsplit('\n')
            .next()
            .map_or(1, |last_line| last_line.chars().count() + 1);
        format!("{message} na linha {line}, coluna {column}")
    }
}

fn parse_json(input: &str) -> Result<JsonValue, String> {
    JsonParser::new(input).parse()
}

// =========================================================================== tempo

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Formata um timestamp Unix (segundos) como `[dia/mês hora:min]` (local).
fn format_epoch(seconds: i64, with_seconds: bool) -> String {
    if seconds == 0 {
        return if with_seconds {
            "01/01 00:00:00".to_owned()
        } else {
            "01/01 00:00".to_owned()
        };
    }
    let fmt = if with_seconds {
        "%d/%m %H:%M:%S"
    } else {
        "%d/%m %H:%M"
    };
    Command::new("date")
        .arg("-d")
        .arg(format!("@{seconds}"))
        .arg(format!("+{fmt}"))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            if with_seconds {
                "01/01 00:00:00".to_owned()
            } else {
                "01/01 00:00".to_owned()
            }
        })
}

/// Trunca um float para 6 casas e remove zeros à direita (aproxima o `%g` do Python).
fn fmt_g(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

// =========================================================================== estado

/// Contexto/tamanho + sinais de trabalho em voo, por sessão (somente metadados).
#[derive(Debug, Clone, Default)]
struct Session {
    session: String,
    ctx: i64,
    t: i64,
    /// Timestamp do último `assistant` (paridade com o Python; não usado na saída).
    #[allow(dead_code)]
    t_assistant: i64,
    finish: Option<String>,
    kids: usize,
    pend: i64,
    em_voo: Vec<String>,
    parada: bool,
    comp_t: i64,
    comp_status: String,
    compactada_sem_uso: bool,
    title: String,
    directory: String,
    ocioso_min: f64,
}

struct Message {
    t: i64,
    kind: String,
    data: JsonValue,
}

/// Lê o banco via `sqlite3 -readonly -json` e devolve as linhas como JSON.
fn sqlite_rows(db: &str, sql: &str) -> Result<Vec<JsonValue>, String> {
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json", db, sql])
        .output()
        .map_err(|error| format!("não foi possível executar sqlite3: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr
            .lines()
            .next()
            .unwrap_or("sqlite3 falhou")
            .trim()
            .trim_start_matches("Parse error in 4th command line argument: ")
            .to_owned());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    parse_json(text)
        .map_err(|error| format!("saída JSON inválida do sqlite3: {error}"))?
        .as_array()
        .map(<[JsonValue]>::to_vec)
        .ok_or_else(|| "saída do sqlite3 não é uma lista JSON".to_owned())
}

fn field_str(row: &JsonValue, key: &str) -> String {
    row.get(key)
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn field_i64(row: &JsonValue, key: &str) -> i64 {
    row.get(key).and_then(JsonValue::as_f64).unwrap_or(0.0) as i64
}

fn load_sessions(db: &str, idle_window: i64, titulos: bool) -> Result<Vec<Session>, String> {
    if sqlite_rows(db, "select 1 from session_message limit 1").is_err() {
        return Err(format!(
            "schema inesperado em {db}\nO OpenCode pode ter mudado o schema nesta versão — verifique com:\n  sqlite3 \"{db}\" \".tables\""
        ));
    }

    let mut meta: BTreeMap<String, (String, String)> = BTreeMap::new();
    for row in sqlite_rows(db, "select id, directory, title from session_v2")? {
        let sid = field_str(&row, "id");
        let directory = field_str(&row, "directory");
        let title = if titulos {
            field_str(&row, "title")
        } else {
            String::new()
        };
        meta.insert(sid, (directory, title));
    }

    let mut filhos: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in sqlite_rows(
        db,
        "select id, parent_id from session_v2 where parent_id is not null",
    )? {
        filhos
            .entry(field_str(&row, "parent_id"))
            .or_default()
            .push(field_str(&row, "id"));
    }

    let mut pend: BTreeMap<String, i64> = BTreeMap::new();
    for tabela in ["session_pending", "session_inbox"] {
        let sql = format!("select session_id, count(*) as n from {tabela} group by session_id");
        let Ok(rows) = sqlite_rows(db, &sql) else {
            continue;
        };
        for row in rows {
            let sid = field_str(&row, "session_id");
            let n = row
                .get("n")
                .or_else(|| row.get("count(*)"))
                .and_then(JsonValue::as_f64)
                .unwrap_or(0.0) as i64;
            *pend.entry(sid).or_insert(0) += n;
        }
    }

    let mut messages: BTreeMap<String, Vec<Message>> = BTreeMap::new();
    let mut ordem_sessoes: Vec<String> = Vec::new();
    let mut kids_last: BTreeMap<String, i64> = BTreeMap::new();
    for row in sqlite_rows(
        db,
        "select session_id, time_created, type, data from session_message order by time_created",
    )? {
        let sid = field_str(&row, "session_id");
        let kind = field_str(&row, "type");
        let data = parse_json(&field_str(&row, "data")).unwrap_or(JsonValue::Null);
        let t = field_i64(&row, "time_created");
        kids_last.insert(sid.clone(), t);
        if !messages.contains_key(&sid) {
            ordem_sessoes.push(sid.clone());
        }
        messages
            .entry(sid)
            .or_default()
            .push(Message { t, kind, data });
    }

    let agora = now_ms();
    let mut out = Vec::new();
    for sid in &ordem_sessoes {
        let list = &messages[sid];
        let mut last_ctx = 0i64;
        let mut last_msg: Option<(i64, Option<String>)> = None;
        let mut comp: Option<(i64, String)> = None;
        let mut ativo = 0i64;
        for message in list {
            if message.kind == "compaction" {
                let status = message
                    .data
                    .get("status")
                    .and_then(JsonValue::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("?")
                    .to_owned();
                comp = Some((message.t, status));
                continue;
            }
            if message.kind == "user" || message.kind == "assistant" {
                ativo = ativo.max(message.t);
            }
            if message.kind != "assistant" {
                continue;
            }
            let model = message
                .data
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(JsonValue::as_str)
                .or_else(|| message.data.get("modelID").and_then(JsonValue::as_str))
                .unwrap_or("");
            let tokens = message.data.get("tokens");
            if tokens.is_some() && model.to_lowercase().contains("deepseek") {
                let input = tokens
                    .and_then(|tokens| tokens.get("input"))
                    .and_then(JsonValue::as_f64)
                    .unwrap_or(0.0);
                let cache_read = tokens
                    .and_then(|tokens| tokens.get("cache"))
                    .and_then(|cache| cache.get("read"))
                    .and_then(JsonValue::as_f64)
                    .unwrap_or(0.0);
                let ctx = input as i64 + cache_read as i64;
                if ctx > 0 {
                    last_ctx = ctx;
                }
            }
            last_msg = Some((
                message.t,
                message
                    .data
                    .get("finish")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned),
            ));
        }

        if last_ctx == 0 {
            continue;
        }
        let (finish_t, finish) = last_msg.unwrap_or((0, None));
        let limite_abs = agora - idle_window * 1000;
        let limite_pai = finish_t;
        let kids_last = kids_last.get(sid).copied().unwrap_or(0);
        let ativos: Vec<&String> = filhos
            .get(sid)
            .into_iter()
            .flatten()
            .filter(|_kid| {
                let kid_t = kids_last;
                kid_t >= limite_abs && kid_t >= limite_pai
            })
            .collect();
        let mut em_voo = Vec::new();
        if finish.as_deref() == Some("tool-calls") {
            em_voo.push("turno em andamento".to_owned());
        }
        if !ativos.is_empty() {
            em_voo.push(format!("{} subagente(s) ativo(s)", ativos.len()));
        }
        let pend_count = pend.get(sid).copied().unwrap_or(0);
        if pend_count > 0 {
            em_voo.push(format!("{pend_count} prompt(s) na fila"));
        }
        let (comp_t, comp_status) = comp.clone().unwrap_or((0, String::new()));
        let t_atv = if ativo != 0 {
            ativo
        } else if finish_t != 0 {
            finish_t
        } else {
            0
        };
        let compactada_sem_uso = comp_t != 0 && comp_t >= t_atv;
        let (directory, title) = meta.get(sid).cloned().unwrap_or_default();
        out.push(Session {
            session: sid.clone(),
            ctx: last_ctx,
            t: t_atv,
            t_assistant: finish_t,
            finish,
            kids: ativos.len(),
            pend: pend_count,
            em_voo,
            parada: t_atv != 0 && (agora - t_atv) > 24 * 3600 * 1000,
            comp_t,
            comp_status,
            compactada_sem_uso,
            title,
            directory,
            ocioso_min: 0.0,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.t));
    Ok(out)
}

// =========================================================================== funções puras

/// `600k` → 600_000 ; `1M` → 1_000_000 ; `900000` → 900_000.
fn parse_size(s: &str) -> Result<i64, String> {
    let text = s.trim();
    let (digits, suffix) = split_number(text);
    let digits = digits.ok_or_else(|| format!("tamanho inválido: {s:?} (use 600k, 1M, 900000)"))?;
    let value: f64 = digits
        .parse()
        .map_err(|_| format!("tamanho inválido: {s:?} (use 600k, 1M, 900000)"))?;
    let mult = match suffix.to_lowercase().as_str() {
        "" => 1.0,
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => return Err(format!("tamanho inválido: {s:?} (use 600k, 1M, 900000)")),
    };
    Ok((value * mult) as i64)
}

/// `15` → 15 min ; `15min` → 15 ; `1h` → 60 ; `90s` → 1.5.
fn parse_min(s: &str) -> Result<f64, String> {
    let text = s.trim().to_lowercase();
    let (digits, suffix) = split_number(&text);
    let digits = digits.ok_or_else(|| format!("tempo inválido: {s:?} (use 15, 15min, 1h, 90s)"))?;
    if !suffix.is_empty() && !suffix.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(format!("tempo inválido: {s:?} (use 15, 15min, 1h, 90s)"));
    }
    let value: f64 = digits
        .parse()
        .map_err(|_| format!("tempo inválido: {s:?} (use 15, 15min, 1h, 90s)"))?;
    if suffix.starts_with('h') {
        Ok(value * 60.0)
    } else if suffix.starts_with('s') {
        Ok(value / 60.0)
    } else {
        Ok(value)
    }
}

/// Divide `<dígitos>.<dígitos><sufixo>` — o número mais o sufixo restante.
fn split_number(text: &str) -> (Option<&str>, &str) {
    let bytes = text.as_bytes();
    let mut end = 0;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        let mut fraction = end + 1;
        while fraction < bytes.len() && bytes[fraction].is_ascii_digit() {
            fraction += 1;
        }
        if fraction > end + 1 {
            end = fraction;
        }
    }
    if end == 0 {
        (None, "")
    } else {
        (Some(&text[..end]), &text[end..])
    }
}

/// `600_000` → `600k`, `1_000_000` → `1.00M`.
fn human(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Idade legível: `45s`, `12min`, `3h05`. `0` → `-`.
fn idade(ms: i64) -> String {
    if ms == 0 {
        return "-".to_owned();
    }
    let d = (now_ms() / 1000 - ms / 1000).max(0);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}min", d / 60)
    } else {
        format!("{}h{:02}", d / 3600, (d % 3600) / 60)
    }
}

/// Uma linha compacta (ASCII) para barras/painéis (ex.: tclock). Sem linhas de log.
fn linha_status(sessions: &[Session], teto: i64) -> String {
    let recentes: Vec<&Session> = sessions.iter().filter(|s| !s.parada).collect();
    if recentes.is_empty() {
        return "nenhuma sessao recente".to_owned();
    }
    let vivas: Vec<&&Session> = recentes.iter().filter(|s| !s.compactada_sem_uso).collect();
    let resumidas = recentes.len() - vivas.len();
    if vivas.is_empty() {
        return format!("{resumidas} resumida(s) | 0 acima de {}", human(teto));
    }
    let maior = vivas
        .iter()
        .max_by_key(|s| s.ctx)
        .expect("vivas não está vazio");
    let acima = vivas.iter().filter(|s| s.ctx >= teto).count();
    let est = if !maior.em_voo.is_empty() {
        format!(" em voo({}x)", maior.em_voo.len())
    } else if maior.parada {
        " parada".to_owned()
    } else {
        String::new()
    };
    format!(
        "maior {}{est} | {acima} acima de {} | {} recentes",
        human(maior.ctx),
        human(teto),
        recentes.len()
    )
}

/// Uma linha curta por sessão (mais recentes primeiro), para painéis/barras.
fn linhas_sessoes(sessions: &[Session], n: i64) -> Vec<String> {
    let take = n.max(0) as usize;
    sessions
        .iter()
        .take(take)
        .map(|s| {
            let mut partes = Vec::new();
            if s.finish.as_deref() == Some("tool-calls") {
                partes.push("turno".to_owned());
            }
            if s.kids != 0 {
                partes.push(format!("sub{}", s.kids));
            }
            if s.pend != 0 {
                partes.push(format!("fila{}", s.pend));
            }
            let est = if !partes.is_empty() {
                partes.join(" ")
            } else if s.compactada_sem_uso {
                "resumida".to_owned()
            } else if s.parada {
                "parada".to_owned()
            } else {
                "livre".to_owned()
            };
            let short: String = s.session.chars().take(16).collect();
            format!("{short:<16} {:>7}  {est}", human(s.ctx))
        })
        .collect()
}

/// Sessões que podem ser compactadas agora: acima do teto, sem trabalho em voo e já ociosas.
fn elegiveis(sessions: &[Session], teto: i64, ocioso_min: f64, agora_ms: i64) -> Vec<Session> {
    let mut out = Vec::new();
    for session in sessions {
        if session.ctx < teto || !session.em_voo.is_empty() {
            continue;
        }
        if session.compactada_sem_uso {
            continue;
        }
        if session.comp_t != 0
            && session.comp_status == "failed"
            && (agora_ms - session.comp_t) as f64 / 60_000.0 < BACKOFF_MIN
        {
            continue;
        }
        let idade = if session.t != 0 {
            (agora_ms - session.t) as f64 / 60_000.0
        } else {
            1e9
        };
        if idade < ocioso_min {
            continue;
        }
        let mut copia = session.clone();
        copia.ocioso_min = idade;
        out.push(copia);
    }
    out
}

// =========================================================================== resolução

/// `--db` > `$OPENCODE_COMPACT_DB` > `opencode debug paths` (linha `db <path>`) > padrões por SO.
fn resolver_db(explicito: Option<&str>) -> String {
    if let Some(path) = explicito.filter(|value| !value.is_empty()) {
        return expand_home(path).display().to_string();
    }
    if let Ok(value) = env::var("OPENCODE_COMPACT_DB")
        && !value.is_empty()
    {
        return expand_home(&value).display().to_string();
    }
    if let Ok(output) = Command::new("opencode").args(["debug", "paths"]).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("db ") {
                let path = rest.trim();
                if !path.is_empty() && !path.contains(char::is_whitespace) {
                    return path.to_owned();
                }
            }
        }
    }
    if cfg!(target_os = "macos") {
        return expand_home("~/Library/Application Support/opencode/opencode.db")
            .display()
            .to_string();
    }
    if cfg!(target_os = "windows") {
        let base = env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| expand_home("~/AppData/Local").display().to_string());
        return PathBuf::from(base)
            .join("opencode")
            .join("opencode.db")
            .display()
            .to_string();
    }
    expand_home("~/.local/share/opencode/opencode.db")
        .display()
        .to_string()
}

/// `--bin` > `$OPENCODE_COMPACT_BIN` > `opencode`.
fn resolver_bin(explicito: Option<&str>) -> String {
    explicito
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            env::var("OPENCODE_COMPACT_BIN")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_BIN.to_owned())
}

fn expand_home(path: &str) -> PathBuf {
    let home = || env::var_os("HOME").map(PathBuf::from);
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = home()
    {
        return home.join(relative);
    }
    PathBuf::from(path)
}

/// Pede a compactação via API local (roda no próximo ponto seguro, funde pedidos repetidos).
fn request_compaction(session: &str, timeout_secs: u64, binario: &str) -> (bool, String) {
    request_compaction_with(session, binario, |command| {
        let _ = timeout_secs;
        command.output()
    })
}

/// Indireção testável sobre o subprocesso da API.
fn request_compaction_with<F>(session: &str, binario: &str, run: F) -> (bool, String)
where
    F: FnOnce(&mut Command) -> io::Result<std::process::Output>,
{
    let mut command = Command::new(binario);
    command.args([
        "api",
        "POST",
        &format!("/api/session/{session}/compact"),
        "-d",
        "{}",
    ]);
    let output = match run(&mut command) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (false, format!("binário `{binario}` não encontrado"));
        }
        Err(error) => return (false, format!("falha ao chamar a API: {error}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let trimmed_stdout = stdout.trim();
    let out = if trimmed_stdout.is_empty() {
        stderr.trim()
    } else {
        trimmed_stdout
    };
    let ok = output.status.success() && !out.to_lowercase().contains("error");
    (ok, out.chars().take(300).collect())
}

// =========================================================================== varredura

/// Uma passada: lista e (com `--apply`) compacta as sessões elegíveis. Usada por `--once`/`--watch`.
fn varredura(
    args: &Options,
    binario: &str,
    logf: &mut Option<std::fs::File>,
) -> Result<usize, String> {
    let mut escreve = |text: &str| {
        println!("{text}");
        if let Some(file) = logf {
            let _ = writeln!(file, "{text}");
            let _ = file.flush();
        }
    };
    let agora = now_ms();
    let sessoes = load_sessions(&args.db, args.idle_window, !args.no_titles)?;
    let acima = sessoes
        .iter()
        .filter(|s| s.ctx >= args.above && !s.compactada_sem_uso)
        .count();
    let resumidas = sessoes.iter().filter(|s| s.compactada_sem_uso).count();
    let alvos = elegiveis(&sessoes, args.above, args.ocioso, agora);
    let mut cabecalho = format!(
        "{}  teto={}  acima={acima}  ocioso>={}min  elegiveis={}  modo={}",
        format_epoch(agora / 1000, true),
        human(args.above),
        fmt_g(args.ocioso),
        alvos.len(),
        if args.apply { "APPLY" } else { "aviso" }
    );
    if resumidas > 0 {
        cabecalho.push_str(&format!("  resumidas={resumidas}"));
    }
    escreve(&cabecalho);
    for s in &alvos {
        escreve(&format!(
            "  alvo {} ctx={} ocioso={:.0}min titulo=\"{}\"",
            s.session.chars().take(26).collect::<String>(),
            human(s.ctx),
            s.ocioso_min,
            s.title.chars().take(34).collect::<String>()
        ));
        if !args.apply {
            escreve("  -> [aviso] compactaria agora");
            continue;
        }
        let (ok, msg) = request_compaction(&s.session, 60, binario);
        escreve(&format!(
            "  -> {}: {}",
            if ok { "pedido aceito" } else { "FALHOU" },
            msg.chars().take(120).collect::<String>()
        ));
    }
    Ok(alvos.len())
}

// =========================================================================== CLI

#[derive(Default)]
struct Options {
    above: i64,
    apply: bool,
    force: bool,
    session: Option<String>,
    all: bool,
    list: bool,
    sessions: i64,
    once: bool,
    watch: i64,
    ocioso: f64,
    log: Option<String>,
    status: bool,
    tui: bool,
    no_titles: bool,
    idle_window: i64,
    limit: i64,
    db: String,
    bin: String,
}

const USAGE: &str = "\
Uso: opencode-compact-if-big [OPÇÕES]

  --above SIZE      teto de contexto (600k, 1M, 900000; padrão: 600k)
  --apply           age de fato (padrão: dry-run)
  --force           age mesmo com trabalho em voo (perigoso)
  --session ID      sessão específica (padrão: a mais recente acima do teto)
  --all             considera todas as sessões acima do teto
  --list            apenas lista as sessões e tamanhos
  --sessions N      com --status: lista até N sessões (uma linha cada)
  --once            uma varredura e sai (systemd/cron)
  --watch SEG       varredura contínua a cada SEG segundos (Ctrl-C sai)
  --ocioso MIN      só age em sessão ociosa há >= MIN (padrão: 15)
  --log FILE        arquivo de log (append) para --once/--watch
  --status          uma linha compacta (para painéis/barras); não age
  --tui             interface interativa (modo raw + ANSI)
  --no-titles       não exibe títulos de sessão (privacidade)
  --idle-window S   janela (s) para considerar subagente ativo (padrão: 600)
  --limit N         linhas no relatório (padrão: 10)
  --db PATH         caminho do opencode.db (padrão: autodetectado)
  --bin NAME        binário que fala com a API da instância (padrão: opencode)
  -V, --version     mostra a versão
  -h, --help        mostra esta ajuda";

fn parse_args(argv: &[String]) -> Result<Options, String> {
    let mut args = Options {
        above: 600_000,
        ocioso: 15.0,
        idle_window: 600,
        limit: 10,
        db: String::new(),
        bin: String::new(),
        ..Options::default()
    };
    let mut iter = argv.iter();
    macro_rules! value {
        ($flag:expr, $iter:expr) => {
            $iter
                .next()
                .ok_or_else(|| format!("faltou o valor após {}", $flag))?
        };
    }
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--above" => args.above = parse_size(value!("--above", iter))?,
            "--apply" => args.apply = true,
            "--force" => args.force = true,
            "--session" => args.session = Some(value!("--session", iter).clone()),
            "--all" => args.all = true,
            "--list" => args.list = true,
            "--sessions" => {
                args.sessions = value!("--sessions", iter)
                    .parse()
                    .map_err(|_| "--sessions espera um inteiro".to_owned())?
            }
            "--once" => args.once = true,
            "--watch" => {
                args.watch = value!("--watch", iter)
                    .parse()
                    .map_err(|_| "--watch espera um inteiro (segundos)".to_owned())?
            }
            "--ocioso" => args.ocioso = parse_min(value!("--ocioso", iter))?,
            "--log" => args.log = Some(value!("--log", iter).clone()),
            "--status" => args.status = true,
            "--tui" => args.tui = true,
            "--no-titles" => args.no_titles = true,
            "--idle-window" => {
                args.idle_window = value!("--idle-window", iter)
                    .parse()
                    .map_err(|_| "--idle-window espera um inteiro (segundos)".to_owned())?
            }
            "--limit" => {
                args.limit = value!("--limit", iter)
                    .parse()
                    .map_err(|_| "--limit espera um inteiro".to_owned())?
            }
            "--db" => args.db = value!("--db", iter).clone(),
            "--bin" => args.bin = value!("--bin", iter).clone(),
            "-V" | "--version" => {
                println!("opencode-compact-if-big {VERSION} (testado com {TESTADO_COM})");
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("argumento desconhecido: {other}\n{USAGE}")),
        }
    }
    Ok(args)
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let code = match run(&argv) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            2
        }
    };
    std::process::exit(code);
}

fn run(argv: &[String]) -> Result<i32, String> {
    let mut args = parse_args(argv)?;
    args.db = resolver_db(
        Some(&args.db)
            .filter(|value| !value.is_empty())
            .map(String::as_str),
    );
    let binario = resolver_bin(
        Some(&args.bin)
            .filter(|value| !value.is_empty())
            .map(String::as_str),
    );

    if args.once || args.watch > 0 {
        let mut logf = None;
        if let Some(path) = &args.log {
            let caminho = expand_home(path);
            if let Some(parent) = caminho.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|error| {
                    format!("não foi possível criar o diretório de log: {error}")
                })?;
            }
            logf = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&caminho)
                    .map_err(|error| format!("não foi possível abrir o log: {error}"))?,
            );
        }
        loop {
            varredura(&args, &binario, &mut logf)?;
            if args.watch <= 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(args.watch as u64));
        }
        return Ok(0);
    }

    if args.status {
        if !Path::new(&args.db).exists() {
            println!("banco ausente: {}", args.db);
            return Ok(2);
        }
        let sessoes = match load_sessions(&args.db, args.idle_window, false) {
            Ok(sessoes) => sessoes,
            Err(error) => {
                println!("{}", error.lines().next().unwrap_or(&error));
                return Ok(2);
            }
        };
        println!("{}", linha_status(&sessoes, args.above));
        let mut ordenadas = sessoes.clone();
        ordenadas.sort_by_key(|s| std::cmp::Reverse(s.t));
        for linha in linhas_sessoes(&ordenadas, args.sessions) {
            println!("{linha}");
        }
        return Ok(0);
    }

    if args.tui {
        if !Path::new(&args.db).exists() {
            eprintln!(
                "M=compactIfBig, E=\"banco ausente: {}\", status=error",
                args.db
            );
            return Ok(2);
        }
        return tui(&args, &binario);
    }

    println!(
        "M=compactIfBig, I=\"iniciando\", teto={}, modo={}, bin={binario}, status=init",
        human(args.above),
        if args.apply { "apply" } else { "dry-run" }
    );

    if !Path::new(&args.db).exists() {
        eprintln!(
            "M=compactIfBig, E=\"banco ausente: {}\" (use --db ou $OPENCODE_COMPACT_DB), status=error",
            args.db
        );
        return Ok(2);
    }

    let mut sessions = load_sessions(&args.db, args.idle_window, !args.no_titles)?;
    if let Some(session) = &args.session {
        let filtradas: Vec<Session> = sessions
            .iter()
            .filter(|s| &s.session == session)
            .cloned()
            .collect();
        sessions = if filtradas.is_empty() {
            vec![Session {
                session: session.clone(),
                title: "(desconhecida)".to_owned(),
                ..Session::default()
            }]
        } else {
            filtradas
        };
    }

    if args.list || sessions.is_empty() {
        for s in sessions.iter().take(args.limit.max(0) as usize) {
            let quando = if s.t != 0 {
                format_epoch(s.t / 1000, false)
            } else {
                "-".to_owned()
            };
            let est = if !s.em_voo.is_empty() {
                format!("EM VOO: {}", s.em_voo.join("; "))
            } else if s.parada {
                "parada".to_owned()
            } else {
                "livre".to_owned()
            };
            println!(
                "  {:<28} {:>8}  {quando}  {}  {}",
                s.session.chars().take(26).collect::<String>(),
                human(s.ctx),
                est.chars().take(60).collect::<String>(),
                s.title.chars().take(28).collect::<String>()
            );
        }
        if sessions.is_empty() {
            println!("  (nenhuma sessão com tokens registrados)");
        }
        return Ok(0);
    }

    let mut alvos: Vec<Session> = sessions
        .iter()
        .filter(|s| s.ctx >= args.above)
        .cloned()
        .collect();
    if alvos.is_empty() {
        println!(
            "M=compactIfBig, I=\"nada a fazer\", maior={}, status=complete",
            human(sessions[0].ctx)
        );
        return Ok(0);
    }
    if !args.all {
        alvos.truncate(1);
    }

    let mut rc = 0;
    for s in &alvos {
        println!(
            "  alvo {} contexto={} titulo=\"{}\" cwd={}",
            s.session.chars().take(26).collect::<String>(),
            human(s.ctx),
            s.title.chars().take(40).collect::<String>(),
            s.directory
        );
        if !s.em_voo.is_empty() && !args.force {
            println!(
                "  -> RECUSADO: trabalho em voo ({}). Espere o turno/subagentes terminarem ou use --force (lossy e irreversível).",
                s.em_voo.join("; ")
            );
            rc = 3;
            continue;
        }
        if !s.em_voo.is_empty() && args.force {
            println!(
                "  -> ATENÇÃO: --force com trabalho em voo ({})",
                s.em_voo.join("; ")
            );
        }
        if !args.apply {
            println!(
                "  -> [dry-run] pediria: opencode api POST /api/session/{}/compact",
                s.session
            );
            continue;
        }
        let (ok, msg) = request_compaction(&s.session, 60, &binario);
        println!(
            "  -> {}: {msg}",
            if ok { "pedido aceito" } else { "FALHOU" }
        );
        if !ok {
            rc = 1;
        }
    }
    println!(
        "M=compactIfBig, I=\"concluido\", alvos={}, aplicado={}, status=complete",
        alvos.len(),
        if args.apply { "True" } else { "False" }
    );
    Ok(rc)
}

// =========================================================================== TUI

struct TerminalSession {
    original_mode: String,
}

impl TerminalSession {
    fn new() -> Result<Self, String> {
        let output = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .map_err(|error| format!("não foi possível consultar stty: {error}"))?;
        if !output.status.success() {
            return Err("stty -g falhou; verifique se stdin é um terminal".to_owned());
        }
        let original_mode = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let session = Self { original_mode };
        session
            .enable_raw_mode()
            .map_err(|error| format!("não foi possível ativar modo interativo: {error}"))?;
        print!("\x1b[?1049h\x1b[?25l");
        io::stdout()
            .flush()
            .map_err(|error| format!("não foi possível iniciar a tela TUI: {error}"))?;
        Ok(session)
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        let status = Command::new("stty")
            .args(["raw", "-echo", "min", "0", "time", "5"])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                "stty não conseguiu ativar modo interativo",
            ))
        }
    }

    fn restore_mode(&self) -> io::Result<()> {
        let status = Command::new("stty").arg(&self.original_mode).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("stty não conseguiu restaurar o terminal"))
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore_mode();
        print!("\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Quit,
    Up,
    Down,
    Reload,
    Armar,
    TetoUp,
    TetoDown,
    Ordenar,
    Todas,
    Compactar,
    Forcar,
    Other,
}

fn read_key() -> io::Result<Option<Key>> {
    let mut input = io::stdin().lock();
    let mut first = [0u8; 1];
    if input.read(&mut first)? == 0 {
        return Ok(None);
    }
    let key = match first[0] {
        b'q' | 27 => {
            if first[0] == 27 {
                read_escape_sequence(&mut input)?
            } else {
                Key::Quit
            }
        }
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'r' => Key::Reload,
        b'a' => Key::Armar,
        b'+' | b'=' => Key::TetoUp,
        b'-' => Key::TetoDown,
        b'o' => Key::Ordenar,
        b't' => Key::Todas,
        b'c' => Key::Compactar,
        b'C' => Key::Forcar,
        3 | 4 => Key::Quit,
        _ => Key::Other,
    };
    Ok(Some(key))
}

fn read_escape_sequence(input: &mut impl Read) -> io::Result<Key> {
    let mut sequence = [0u8; 2];
    if input.read(&mut sequence[..1])? == 0 || sequence[0] != b'[' {
        return Ok(Key::Other);
    }
    if input.read(&mut sequence[1..])? == 0 {
        return Ok(Key::Other);
    }
    Ok(match sequence[1] {
        b'A' => Key::Up,
        b'B' => Key::Down,
        _ => Key::Other,
    })
}

fn terminal_size() -> (usize, usize) {
    if let Ok(output) = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        && output.status.success()
    {
        let values: Vec<usize> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .filter_map(|value| value.parse().ok())
            .collect();
        if values.len() == 2 && values[0] > 0 && values[1] > 0 {
            return (values[0], values[1]);
        }
    }
    let rows = env::var("LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24);
    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80);
    (rows, columns)
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn pad_display(value: &str, width: usize, right_align: bool) -> String {
    let text = take_chars(value, width);
    let len = text.chars().count();
    if len >= width {
        text
    } else if right_align {
        format!("{}{text}", " ".repeat(width - len))
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tui(
    sessions: &[Session],
    sel: usize,
    armado: bool,
    ordem: &str,
    todas: bool,
    teto: i64,
    msg: &str,
    rows: usize,
    columns: usize,
) -> io::Result<()> {
    let mut output = String::from("\x1b[2J\x1b[H");
    let titulo = format!(
        " Compactação por teto — OpenCode v{VERSION}   teto={}   [{}]   {ordem}   {}",
        human(teto),
        if armado { "ARMADO" } else { "SIMULAÇÃO" },
        if todas { "todas" } else { "recentes(24h)" }
    );
    output.push_str("\x1b[7m");
    output.push_str(&pad_display(&titulo, columns.saturating_sub(1), false));
    output.push_str("\x1b[0m\r\n");
    output.push_str("\x1b[1m");
    output.push_str(&pad_display(
        &format!(
            "{:>2} {:<26} {:>9} {:<28} {:>7}  título",
            "", "sessão", "contexto", "estado", "visto"
        ),
        columns.saturating_sub(1),
        false,
    ));
    output.push_str("\x1b[0m\r\n");

    let visiveis = rows.saturating_sub(5).max(1);
    let inicio = if sel > visiveis / 3 {
        (sel - visiveis / 3).min(sessions.len().saturating_sub(visiveis))
    } else {
        0
    };
    let mut used = 2usize;
    for (i, s) in sessions.iter().enumerate().skip(inicio).take(visiveis) {
        let est = if !s.em_voo.is_empty() {
            s.em_voo.join(", ")
        } else if s.compactada_sem_uso {
            "resumida (sem uso novo)".to_owned()
        } else if s.parada {
            "parada".to_owned()
        } else {
            "livre".to_owned()
        };
        let title_width = columns.saturating_sub(80);
        let linha = format!(
            "{} {:<26} {:>9} {:<28} {:>7}  {}",
            if i == sel { "→" } else { " " },
            take_chars(&s.session, 26),
            human(s.ctx),
            take_chars(&est, 28),
            idade(s.t),
            take_chars(&s.title, title_width)
        );
        if i == sel {
            output.push_str("\x1b[7m");
            output.push_str(&pad_display(&linha, columns.saturating_sub(1), false));
            output.push_str("\x1b[0m");
        } else if !s.em_voo.is_empty() && !armado {
            output.push_str("\x1b[31m");
            output.push_str(&pad_display(&linha, columns.saturating_sub(1), false));
            output.push_str("\x1b[0m");
        } else if s.ctx >= teto {
            output.push_str("\x1b[1m");
            output.push_str(&pad_display(&linha, columns.saturating_sub(1), false));
            output.push_str("\x1b[0m");
        } else {
            output.push_str(&pad_display(&linha, columns.saturating_sub(1), false));
        }
        output.push_str("\r\n");
        used += 1;
    }
    if sessions.is_empty() {
        output.push_str("\r\n  nenhuma sessão recente (use 't' para mostrar as paradas)\r\n");
        used += 2;
    }

    let mut remaining = rows.saturating_sub(used + 2);
    while remaining > 0 {
        output.push_str("\r\n");
        remaining -= 1;
    }
    output.push_str("\x1b[2m");
    output.push_str(&pad_display(
        "↑/↓ mover · r atualizar · a armar · +/- teto · o ordenar · t todas · c compactar · C forçar · q sair",
        columns.saturating_sub(1),
        false,
    ));
    output.push_str("\x1b[0m\r\n");
    output.push_str(&pad_display(
        if msg.is_empty() { " " } else { msg },
        columns.saturating_sub(1),
        false,
    ));
    output.push_str("\r\n");
    io::stdout().write_all(output.as_bytes())?;
    io::stdout().flush()
}

fn recarregar(
    db: &str,
    idle_window: i64,
    no_titles: bool,
    ordem: &str,
    todas: bool,
) -> (Vec<Session>, Option<String>) {
    match load_sessions(db, idle_window, !no_titles) {
        Ok(mut sessions) => {
            if ordem == "ctx" {
                sessions.sort_by_key(|s| std::cmp::Reverse(s.ctx));
            } else {
                sessions.sort_by_key(|s| std::cmp::Reverse(s.t));
            }
            if !todas {
                sessions.retain(|s| !s.parada);
            }
            (sessions, None)
        }
        Err(error) => (
            Vec::new(),
            Some(error.lines().next().unwrap_or(&error).to_owned()),
        ),
    }
}

fn ler_resposta(prompt: &str, columns: usize) -> io::Result<String> {
    let mut out = io::stdout();
    let mut line = format!(
        "\r\x1b[2K\x1b[1m{}\x1b[0m ",
        take_chars(prompt, columns.saturating_sub(2))
    );
    if line.chars().count() > columns.saturating_sub(1) {
        line = take_chars(&line, columns.saturating_sub(1));
    }
    out.write_all(line.as_bytes())?;
    out.flush()?;
    let mut resposta = String::new();
    io::stdin().read_line(&mut resposta)?;
    Ok(resposta.trim().to_owned())
}

fn confirmar(prompt: &str, palavra: Option<&str>, columns: usize) -> bool {
    let resposta = match ler_resposta(prompt, columns) {
        Ok(value) => value.to_lowercase(),
        Err(_) => String::new(),
    };
    match palavra {
        Some(word) => resposta == word,
        None => matches!(resposta.as_str(), "s" | "sim" | "y" | "yes"),
    }
}

fn tui(args: &Options, binario: &str) -> Result<i32, String> {
    if env::var("TERM").is_ok_and(|term| term == "dumb") {
        return Err("terminal TERM=dumb não oferece suporte à TUI".to_owned());
    }
    let _session = TerminalSession::new()?;
    let mut teto = args.above;
    let mut sel = 0usize;
    let mut armado = false;
    let mut ordem = "ctx".to_owned();
    let mut msg = String::new();
    let mut todas = false;
    let auto = 20u64;
    let (mut sessions, mut erro) =
        recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
    if let Some(error) = erro.take() {
        msg = error;
    }
    let mut carregado = now_ms();
    let mut last_tick = now_ms();

    loop {
        let (rows, columns) = terminal_size();
        draw_tui(
            &sessions, sel, armado, &ordem, todas, teto, &msg, rows, columns,
        )
        .map_err(|error| error.to_string())?;
        if now_ms() - carregado > (auto * 1000) as i64 {
            (sessions, erro) =
                recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
            if let Some(error) = erro.take() {
                msg = error;
            }
            carregado = now_ms();
        }
        match read_key().map_err(|error| format!("erro de leitura do terminal: {error}"))? {
            None => {
                // stty já bloqueia 5s no read; mantém o laço vivo.
                std::thread::sleep(std::time::Duration::from_millis(
                    50.min(last_tick.max(0) as u64),
                ));
                last_tick = now_ms();
            }
            Some(Key::Quit) => return Ok(0),
            Some(Key::Up) => sel = sel.saturating_sub(1),
            Some(Key::Down) => {
                if !sessions.is_empty() {
                    sel = (sel + 1).min(sessions.len() - 1);
                }
            }
            Some(Key::Reload) => {
                (sessions, erro) =
                    recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
                msg = erro.take().unwrap_or_else(|| "atualizado".to_owned());
                carregado = now_ms();
            }
            Some(Key::Armar) => {
                armado = !armado;
                msg = if armado {
                    "ARMADO: c vai pedir compactação de verdade".to_owned()
                } else {
                    "SIMULAÇÃO: nada é enviado".to_owned()
                };
            }
            Some(Key::TetoUp) => teto = (teto + 100_000).min(1_000_000),
            Some(Key::TetoDown) => teto = (teto - 100_000).max(0),
            Some(Key::Ordenar) => {
                ordem = if ordem == "ctx" { "recencia" } else { "ctx" }.to_owned();
                (sessions, erro) =
                    recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
                if let Some(error) = erro.take() {
                    msg = error;
                }
            }
            Some(Key::Todas) => {
                todas = !todas;
                (sessions, erro) =
                    recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
                if let Some(error) = erro.take() {
                    msg = error;
                }
            }
            Some(Key::Compactar) => {
                agir(
                    &sessions, sel, armado, false, args, binario, &mut msg, columns,
                );
                (sessions, erro) =
                    recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
                if let Some(error) = erro.take() {
                    msg = error;
                }
                carregado = now_ms();
            }
            Some(Key::Forcar) => {
                agir(
                    &sessions, sel, armado, true, args, binario, &mut msg, columns,
                );
                (sessions, erro) =
                    recarregar(&args.db, args.idle_window, args.no_titles, &ordem, todas);
                if let Some(error) = erro.take() {
                    msg = error;
                }
                carregado = now_ms();
            }
            Some(Key::Other) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn agir(
    sessions: &[Session],
    sel: usize,
    armado: bool,
    forcar: bool,
    args: &Options,
    binario: &str,
    msg: &mut String,
    columns: usize,
) {
    let Some(s) = sessions.get(sel) else {
        return;
    };
    if !s.em_voo.is_empty() && !forcar {
        *msg = format!(
            "RECUSADO: {} — use C para forçar (lossy!)",
            s.em_voo.join(", ")
        );
        return;
    }
    if !armado && !forcar {
        *msg = format!(
            "SIMULAÇÃO: pediria compactar {} ({}). Pressione 'a' para armar.",
            take_chars(&s.session, 20),
            human(s.ctx)
        );
        return;
    }
    let risco = if !s.em_voo.is_empty() {
        " ATENÇÃO: trabalho em voo!"
    } else {
        ""
    };
    let session = take_chars(&s.session, 20);
    let ok = if forcar && !s.em_voo.is_empty() {
        confirmar(
            &format!(
                "Compactar {session} ({})?{risco} digite \"force\":",
                human(s.ctx)
            ),
            Some("force"),
            columns,
        )
    } else {
        confirmar(
            &format!("Compactar {session} ({})?{risco} [s/N]", human(s.ctx)),
            None,
            columns,
        )
    };
    if !ok {
        *msg = "cancelado".to_owned();
        return;
    }
    let _ = args;
    let (ok, resposta) = request_compaction(&s.session, 60, binario);
    *msg = format!(
        "{}: {}",
        if ok { "pedido aceito" } else { "FALHOU" },
        take_chars(&resposta, 80)
    );
}

// =========================================================================== testes unitários

#[cfg(test)]
mod tests {
    use super::*;

    fn sessao_livre(sid: &str, ctx: i64, t: i64) -> Session {
        Session {
            session: sid.to_owned(),
            ctx,
            t,
            finish: Some("stop".to_owned()),
            ..Session::default()
        }
    }

    #[test]
    fn parse_size_aceita_formatos_da_cli() {
        assert_eq!(parse_size("600k").unwrap(), 600_000);
        assert_eq!(parse_size("1M").unwrap(), 1_000_000);
        assert_eq!(parse_size("900000").unwrap(), 900_000);
        assert_eq!(parse_size(" 600K ").unwrap(), 600_000);
        assert_eq!(parse_size("1.5m").unwrap(), 1_500_000);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn parse_min_aceita_formatos_da_cli() {
        assert_eq!(parse_min("15").unwrap(), 15.0);
        assert_eq!(parse_min("15min").unwrap(), 15.0);
        assert_eq!(parse_min("1h").unwrap(), 60.0);
        assert!((parse_min("90s").unwrap() - 1.5).abs() < 1e-9);
        assert!(parse_min("abc").is_err());
    }

    #[test]
    fn human_espelha_o_python() {
        assert_eq!(human(600_000), "600k");
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(1_000), "1k");
        assert_eq!(human(999_999), "1000k");
        assert_eq!(human(1_000_000), "1.00M");
        assert_eq!(human(1_234_567), "1.23M");
    }

    #[test]
    fn idade_tem_unidades_compactas() {
        let agora = now_ms();
        assert_eq!(idade(0), "-");
        assert_eq!(idade(agora - 5_000), "5s");
        assert_eq!(idade(agora - 12 * 60_000), "12min");
        assert_eq!(idade(agora - 3 * 3_600_000 - 5 * 60_000), "3h05");
        assert_eq!(idade(agora + 10_000), "0s");
    }

    #[test]
    fn linha_status_resume_sessoes_recentes() {
        let s = vec![
            Session {
                ctx: 800_000,
                em_voo: vec!["1 subagente(s) ativo(s)".to_owned()],
                ..sessao_livre("a", 800_000, 1)
            },
            sessao_livre("b", 700_000, 2),
            Session {
                parada: true,
                ..sessao_livre("c", 900_000, 3)
            },
        ];
        let linha = linha_status(&s, 600_000);
        assert!(linha.contains("maior 800k"), "{linha}");
        assert!(linha.contains("em voo(1x)"), "{linha}");
        assert!(linha.contains("2 acima de 600k"), "{linha}");
        assert!(linha.contains("2 recentes"), "{linha}");
    }

    #[test]
    fn linha_status_sem_recentes() {
        assert_eq!(linha_status(&[], 600_000), "nenhuma sessao recente");
        let paradas = vec![Session {
            parada: true,
            ..sessao_livre("a", 900_000, 1)
        }];
        assert_eq!(linha_status(&paradas, 600_000), "nenhuma sessao recente");
    }

    #[test]
    fn linha_status_compactada_nao_conta_acima_do_teto() {
        let s = vec![
            Session {
                compactada_sem_uso: true,
                ..sessao_livre("a", 733_000, 1)
            },
            sessao_livre("b", 200_000, 2),
        ];
        let linha = linha_status(&s, 600_000);
        assert!(linha.contains("0 acima de 600k"), "{linha}");
        assert!(linha.contains("maior 200k"), "{linha}");
    }

    #[test]
    fn linhas_sessoes_mostra_estado_curto() {
        let sess = vec![
            sessao_livre("ses_aaaaaaaaaaaa1", 768_000, 1),
            Session {
                em_voo: vec!["1 sub".to_owned()],
                finish: Some("tool-calls".to_owned()),
                kids: 1,
                ..sessao_livre("ses_bbbbbbbbbbbb2", 639_000, 2)
            },
            Session {
                parada: true,
                ..sessao_livre("ses_cccccccccccc3", 1_000, 3)
            },
        ];
        let linhas = linhas_sessoes(&sess, 2);
        assert_eq!(linhas.len(), 2);
        assert!(linhas[0].contains("768k"));
        assert!(linhas[0].contains("livre"));
        assert!(linhas[1].contains("turno"));
        assert!(linhas[1].contains("sub1"));
        assert!(linhas_sessoes(&sess, 0).is_empty());
    }

    #[test]
    fn elegiveis_exige_acima_livre_e_ociosa() {
        let agora = 1_000_000_000_000i64;
        let sess = vec![
            sessao_livre("a", 700_000, agora - 20 * 60_000),
            Session {
                em_voo: vec!["1 sub".to_owned()],
                ..sessao_livre("b", 700_000, agora - 20 * 60_000)
            },
            sessao_livre("c", 700_000, agora - 5 * 60_000),
            sessao_livre("d", 100_000, agora - 20 * 60_000),
        ];
        let alvos = elegiveis(&sess, 600_000, 15.0, agora);
        assert_eq!(alvos.len(), 1);
        assert_eq!(alvos[0].session, "a");
        assert!((alvos[0].ocioso_min - 20.0).abs() < 0.2);
    }

    #[test]
    fn elegiveis_respeita_compactada_e_backoff() {
        let agora = 1_000_000_000_000i64;
        let compactada = vec![Session {
            comp_t: agora - 30 * 60_000,
            comp_status: "completed".to_owned(),
            compactada_sem_uso: true,
            ..sessao_livre("a", 700_000, agora - 60 * 60_000)
        }];
        assert!(elegiveis(&compactada, 600_000, 15.0, agora).is_empty());

        let falhou = vec![Session {
            comp_t: agora - 10 * 60_000,
            comp_status: "failed".to_owned(),
            ..sessao_livre("a", 700_000, agora - 60 * 60_000)
        }];
        assert!(elegiveis(&falhou, 600_000, 15.0, agora).is_empty());
        assert_eq!(
            elegiveis(&falhou, 600_000, 15.0, agora + 120 * 60_000).len(),
            1
        );
    }

    #[test]
    fn elegiveis_ocioso_zero_pega_livre() {
        let agora = 1_000_000_000_000i64;
        let sess = vec![sessao_livre("a", 700_000, agora)];
        assert_eq!(elegiveis(&sess, 600_000, 0.0, agora).len(), 1);
    }

    #[test]
    fn fmt_g_aproxima_o_percent_g() {
        assert_eq!(fmt_g(15.0), "15");
        assert_eq!(fmt_g(1.5), "1.5");
        assert_eq!(fmt_g(120.0), "120");
        assert_eq!(fmt_g(1.0 / 60.0), "0.016667");
    }

    #[test]
    fn json_parser_le_objetos_do_sqlite() {
        let value = parse_json(r#"[{"a":5,"b":"x"},{"a":10}]"#).unwrap();
        let rows = value.as_array().unwrap();
        assert_eq!(field_i64(&rows[0], "a"), 5);
        assert_eq!(field_str(&rows[0], "b"), "x");
        assert_eq!(field_i64(&rows[1], "a"), 10);
        assert_eq!(field_str(&rows[1], "b"), "");
    }

    #[test]
    fn json_parser_distingue_ausente_do_tipo_errado() {
        let value = parse_json(r#"{"finish":null,"n":3,"cache":{"read":7}}"#).unwrap();
        assert_eq!(value.get("finish").and_then(JsonValue::as_str), None);
        assert_eq!(
            value
                .get("cache")
                .and_then(|c| c.get("read"))
                .and_then(JsonValue::as_f64),
            Some(7.0)
        );
        assert_eq!(value.get("nope"), None);
    }

    #[test]
    fn resolver_bin_precedencia() {
        unsafe { env::remove_var("OPENCODE_COMPACT_BIN") };
        assert_eq!(resolver_bin(Some("opencode-2")), "opencode-2");
        unsafe { env::set_var("OPENCODE_COMPACT_BIN", "opencode-9") };
        assert_eq!(resolver_bin(None), "opencode-9");
        assert_eq!(resolver_bin(Some("opencode-3")), "opencode-3");
        unsafe { env::remove_var("OPENCODE_COMPACT_BIN") };
        assert_eq!(resolver_bin(None), "opencode");
    }

    #[test]
    fn resolver_db_explicito_vence() {
        assert_eq!(resolver_db(Some("/tmp/x.db")), "/tmp/x.db");
    }

    #[test]
    fn request_compaction_usa_o_binario_da_instancia() {
        let mut capturado = Vec::new();
        let (ok, _) = request_compaction_with("ses_alvo", "opencode-2", |command| {
            capturado = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            Ok(std::process::Output {
                status: std::os::unix::process::ExitStatusExt::from_raw(0),
                stdout: b"aceito".to_vec(),
                stderr: Vec::new(),
            })
        });
        assert!(ok);
        assert_eq!(
            env::current_dir().map(|_| ()).ok(),
            Some(()),
            "o teste não pode falhar por diretório"
        );
        assert!(
            capturado
                .iter()
                .any(|arg| arg == "/api/session/ses_alvo/compact"),
            "{capturado:?}"
        );
        assert_eq!(capturado.first().map(String::as_str), Some("api"));
    }
}
