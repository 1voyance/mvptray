//! Редактирование пользовательских правил в config.yaml.
//!
//! Правила живут между маркерами:
//!   # ==CUSTOM_PROCESS_START== … # ==CUSTOM_PROCESS_END==   (PROCESS-NAME)
//!   # ==CUSTOM_DOMAIN_START==  … # ==CUSTOM_DOMAIN_END==    (всё остальное)
//! Файл правится построчно: комментарии, пустые строки, кодировка (BOM) и
//! переводы строк (CRLF/LF) сохраняются. Любое изменение:
//!   1) кладёт копию старого конфига в tray-rs\backups (последние 15);
//!   2) записывает файл атомарно;
//!   3) просит Mihomo перечитать конфиг; если Mihomo отказал — файл откатывается.

use crate::api::{Api, ApiError};
use crate::util;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::IpAddr;
use std::sync::Mutex;

pub const PROC_START: &str = "# ==CUSTOM_PROCESS_START==";
pub const PROC_END: &str = "# ==CUSTOM_PROCESS_END==";
pub const DOM_START: &str = "# ==CUSTOM_DOMAIN_START==";
pub const DOM_END: &str = "# ==CUSTOM_DOMAIN_END==";

const KINDS: &[&str] = &["DOMAIN-SUFFIX", "DOMAIN", "DOMAIN-KEYWORD", "PROCESS-NAME", "IP-CIDR", "IP-CIDR6"];
const MAX_BACKUPS: usize = 15;

/// Служебная группа для теста скорости и хосты тестовых файлов.
pub const PROBE_GROUP: &str = "TrayProbe";
pub const PROBE_HOSTS: &[&str] = &["cachefly.cachefly.net", "speedtest.tele2.net"];

/// Все операции с конфигом идут по очереди.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub kind: String,
    pub value: String,
    pub action: String,
    #[serde(default)]
    pub extra: String,
}

impl Rule {
    /// Ключ правила: тип + значение (без учёта регистра). Одно значение — одно правило.
    pub fn key(&self) -> String {
        format!("{},{}", self.kind, self.value.to_lowercase())
    }

    pub fn to_line(&self, indent: &str) -> String {
        let mut s = format!("{indent}- {},{},{}", self.kind, self.value, self.action);
        if !self.extra.is_empty() {
            s.push(',');
            s.push_str(&self.extra);
        }
        s
    }

    /// Разбирает строку вида "  - DOMAIN-SUFFIX,example.com,Proxy[,no-resolve]  # коммент".
    pub fn parse(line: &str) -> Option<Rule> {
        let t = line.trim();
        let body = t.strip_prefix('-')?.trim();
        let body = match body.find(" #") {
            Some(i) => body[..i].trim(),
            None => body,
        };
        let body = body.trim_matches(|c| c == '"' || c == '\'');
        let mut parts = body.splitn(4, ',').map(str::trim);
        let kind = parts.next()?.to_uppercase();
        let value = parts.next()?.to_string();
        let action = parts.next()?.to_string();
        let extra = parts.next().unwrap_or("").to_string();
        if kind.is_empty() || value.is_empty() || action.is_empty() {
            return None;
        }
        Some(Rule { kind, value, action, extra })
    }

    fn block(&self) -> Block {
        if self.kind == "PROCESS-NAME" { Block::Process } else { Block::Domain }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Block {
    Process,
    Domain,
}

impl Block {
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            Block::Process => (PROC_START, PROC_END),
            Block::Domain => (DOM_START, DOM_END),
        }
    }
}

// ================================================================ документ

/// config.yaml в виде строк с сохранением формата.
#[derive(Clone, Debug)]
pub struct Doc {
    pub lines: Vec<String>,
    crlf: bool,
    bom: bool,
    trailing_newline: bool,
}

impl Doc {
    pub fn parse(text: &str) -> Doc {
        let bom = text.starts_with('\u{feff}');
        let text = text.trim_start_matches('\u{feff}');
        let crlf = text.contains("\r\n");
        let trailing_newline = text.ends_with('\n');
        let lines = text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
        Doc { lines, crlf, bom, trailing_newline }
    }

    pub fn render(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut s = String::new();
        if self.bom {
            s.push('\u{feff}');
        }
        s.push_str(&self.lines.join(eol));
        if self.trailing_newline {
            s.push_str(eol);
        }
        s
    }

    fn find(&self, marker: &str) -> Option<usize> {
        self.lines.iter().position(|l| l.trim() == marker)
    }

    fn range(&self, b: Block) -> Option<(usize, usize)> {
        let (s, e) = b.markers();
        let si = self.find(s)?;
        let ei = self.find(e)?;
        if si < ei { Some((si, ei)) } else { None }
    }

    /// Отступ, которым записаны правила (берём из маркера).
    fn indent(&self, b: Block) -> String {
        self.range(b)
            .map(|(s, _)| {
                let l = &self.lines[s];
                l[..l.len() - l.trim_start().len()].to_string()
            })
            .unwrap_or_else(|| "  ".to_string())
    }

    /// Если маркеров нет — создаём оба блока сразу после строки `rules:`.
    pub fn ensure_blocks(&mut self) -> Result<(), String> {
        if self.range(Block::Process).is_some() && self.range(Block::Domain).is_some() {
            return Ok(());
        }
        let ri = self
            .lines
            .iter()
            .position(|l| l.trim_end() == "rules:")
            .ok_or("в config.yaml нет секции rules:")?;
        let mut insert = Vec::new();
        if self.range(Block::Process).is_none() {
            insert.push(format!("  {PROC_START}"));
            insert.push(format!("  {PROC_END}"));
        }
        if self.range(Block::Domain).is_none() {
            insert.push(format!("  {DOM_START}"));
            insert.push(format!("  {DOM_END}"));
        }
        for (i, l) in insert.into_iter().enumerate() {
            self.lines.insert(ri + 1 + i, l);
        }
        Ok(())
    }

    /// Пользовательские правила в порядке файла (сначала процессы, потом домены).
    pub fn rules(&self) -> Vec<Rule> {
        let mut out = Vec::new();
        for b in [Block::Process, Block::Domain] {
            if let Some((s, e)) = self.range(b) {
                for l in &self.lines[s + 1..e] {
                    if let Some(r) = Rule::parse(l) {
                        out.push(r);
                    }
                }
            }
        }
        out
    }

    /// Индекс строки правила с таким ключом внутри своего блока.
    fn line_of(&self, key: &str) -> Option<(Block, usize)> {
        for b in [Block::Process, Block::Domain] {
            if let Some((s, e)) = self.range(b) {
                for i in s + 1..e {
                    if let Some(r) = Rule::parse(&self.lines[i]) {
                        if r.key() == key {
                            return Some((b, i));
                        }
                    }
                }
            }
        }
        None
    }

    pub fn add(&mut self, r: &Rule) -> Result<bool, String> {
        self.ensure_blocks()?;
        if self.line_of(&r.key()).is_some() {
            return Ok(false); // уже есть
        }
        let b = r.block();
        let (_, e) = self.range(b).ok_or("не найден блок правил")?;
        let ind = self.indent(b);
        self.lines.insert(e, r.to_line(&ind));
        Ok(true)
    }

    pub fn remove(&mut self, key: &str) -> bool {
        match self.line_of(key) {
            Some((_, i)) => {
                self.lines.remove(i);
                true
            }
            None => false,
        }
    }

    /// Замена правила (смена действия/значения) на том же месте.
    pub fn replace(&mut self, key: &str, r: &Rule) -> Result<(), String> {
        let (b, i) = self.line_of(key).ok_or("правило не найдено (возможно, файл изменили)")?;
        if r.key() != key && self.line_of(&r.key()).is_some() {
            return Err(format!("правило для «{}» уже существует", r.value));
        }
        if r.block() != b {
            self.lines.remove(i);
            self.add(r)?;
        } else {
            let ind = self.indent(b);
            self.lines[i] = r.to_line(&ind);
        }
        Ok(())
    }

    /// Сдвиг правила вверх (-1) или вниз (+1) среди правил своего блока.
    pub fn shift(&mut self, key: &str, dir: i32) -> Result<(), String> {
        let (b, i) = self.line_of(key).ok_or("правило не найдено")?;
        let (s, e) = self.range(b).ok_or("не найден блок")?;
        let rule_idx: Vec<usize> = (s + 1..e).filter(|&j| Rule::parse(&self.lines[j]).is_some()).collect();
        let pos = rule_idx.iter().position(|&j| j == i).ok_or("правило не найдено")?;
        let target = pos as i32 + dir;
        if target < 0 || target as usize >= rule_idx.len() {
            return Ok(());
        }
        self.lines.swap(i, rule_idx[target as usize]);
        Ok(())
    }

    /// Первая группа из proxy-groups (запасной вариант, если API недоступен).
    pub fn first_group(&self) -> Option<String> {
        let mut in_groups = false;
        for l in &self.lines {
            let t = l.trim();
            if l.starts_with("proxy-groups:") {
                in_groups = true;
                continue;
            }
            if in_groups {
                if !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('#') && !l.starts_with('-') {
                    break;
                }
                if let Some(v) = t.strip_prefix("- name:") {
                    let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !v.is_empty() && v != PROBE_GROUP {
                        return Some(v.to_string());
                    }
                }
            }
        }
        None
    }

    /// Имена proxy-providers из конфига.
    pub fn provider_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for l in &self.lines {
            if l.starts_with("proxy-providers:") {
                inside = true;
                continue;
            }
            if inside {
                if !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('#') {
                    break;
                }
                // ключ провайдера: ровно два пробела отступа
                if l.starts_with("  ") && !l.starts_with("   ") && l.trim_end().ends_with(':') && !l.trim_start().starts_with('#') {
                    out.push(l.trim().trim_end_matches(':').trim_matches(|c| c == '"' || c == '\'').to_string());
                }
            }
        }
        out
    }

    /// Имена нод из секции proxies: (ручные ноды).
    pub fn static_proxy_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for l in &self.lines {
            if l.starts_with("proxies:") {
                inside = true;
                continue;
            }
            if inside {
                if !l.is_empty() && !l.starts_with(char::is_whitespace) && !l.starts_with('#') && !l.starts_with('-') {
                    break;
                }
                let t = l.trim_start();
                if t.starts_with('#') {
                    continue;
                }
                let indent = l.len() - t.len();
                if indent > 2 || !t.starts_with('-') {
                    continue;
                }
                let rest = t.trim_start_matches('-').trim().trim_start_matches('{').trim();
                if let Some(v) = rest.strip_prefix("name:") {
                    let v = v.trim();
                    let v = if let Some(q) = v.strip_prefix('"') {
                        q.split('"').next().unwrap_or("")
                    } else if let Some(q) = v.strip_prefix('\'') {
                        q.split('\'').next().unwrap_or("")
                    } else {
                        v.split([',', '}']).next().unwrap_or("").trim()
                    };
                    if !v.is_empty() {
                        out.push(v.to_string());
                    }
                }
            }
        }
        out
    }

    /// Удаляет служебную группу (чтобы пересоздать её с актуальным списком нод).
    pub fn remove_probe_group(&mut self) -> bool {
        let head = format!("name: \"{PROBE_GROUP}\"");
        let Some(i) = self.lines.iter().position(|l| l.trim().trim_start_matches("- ").trim() == head) else {
            return false;
        };
        let mut start = i;
        if start > 0 && self.lines[start - 1].contains("служебная группа Mihomo Tray") {
            start -= 1;
        }
        let mut end = i + 1;
        while end < self.lines.len() && self.lines[end].starts_with("    ") {
            end += 1;
        }
        self.lines.drain(start..end);
        true
    }

    /// Служебная группа и правила для теста скорости нод (не влияет на ваш трафик).
    /// Возвращает true, если что-то добавлено.
    pub fn ensure_probe(&mut self) -> Result<bool, String> {
        let mut changed = false;
        let has_group = self.lines.iter().any(|l| l.trim().trim_start_matches("- ").trim() == format!("name: \"{PROBE_GROUP}\""));
        if !has_group {
            let gi = self.lines.iter().position(|l| l.trim_end() == "proxy-groups:").ok_or("в config.yaml нет секции proxy-groups:")?;
            let provs = self.provider_names();
            let mut block = vec![
                "  # --- служебная группа Mihomo Tray для теста скорости нод (не трогать) ---".to_string(),
                format!("  - name: \"{PROBE_GROUP}\""),
                "    type: select".to_string(),
                "    hidden: true".to_string(),
            ];
            if !provs.is_empty() {
                block.push("    use:".into());
                for p in &provs {
                    block.push(format!("      - {p}"));
                }
            }
            block.push("    proxies:".into());
            for n in self.static_proxy_names() {
                block.push(format!("      - \"{}\"", n.replace('"', "")));
            }
            block.push("      - DIRECT".into());
            for (i, l) in block.into_iter().enumerate() {
                self.lines.insert(gi + 1 + i, l);
            }
            changed = true;
        }
        let missing: Vec<&str> = PROBE_HOSTS
            .iter()
            .copied()
            .filter(|h| !self.lines.iter().any(|l| l.trim() == format!("- DOMAIN,{h},{PROBE_GROUP}")))
            .collect();
        if !missing.is_empty() {
            let ri = self.lines.iter().position(|l| l.trim_end() == "rules:").ok_or("в config.yaml нет секции rules:")?;
            let mut ins = vec!["  # служебное правило Mihomo Tray: тест скорости нод".to_string()];
            for h in missing {
                ins.push(format!("  - DOMAIN,{h},{PROBE_GROUP}"));
            }
            for (i, l) in ins.into_iter().enumerate() {
                self.lines.insert(ri + 1 + i, l);
            }
            changed = true;
        }
        Ok(changed)
    }

    /// Булев ключ первого уровня внутри секции: `section_flag("tun", "enable")`
    /// для `tun:\n  enable: true`. Понимает и запись в одну строку `tun: {enable: true}`.
    pub fn section_flag(&self, section: &str, key: &str) -> bool {
        let head = format!("{section}:");
        let Some(start) = self.lines.iter().position(|l| l.starts_with(&head)) else { return false };
        let is_true = |v: &str| {
            let v = match v.find(" #") {
                Some(i) => &v[..i],
                None => v,
            };
            matches!(v.trim().trim_matches(|c| c == '"' || c == '\'').to_lowercase().as_str(), "true" | "yes" | "on")
        };
        let inline = self.lines[start][head.len()..].trim();
        if let Some(body) = inline.strip_prefix('{') {
            return body
                .trim_end_matches('}')
                .split(',')
                .filter_map(|p| p.split_once(':'))
                .any(|(k, v)| k.trim() == key && is_true(v));
        }
        let mut child_indent: Option<usize> = None;
        for l in &self.lines[start + 1..] {
            let t = l.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let indent = l.len() - t.len();
            if indent == 0 {
                break; // началась следующая секция
            }
            let ci = *child_indent.get_or_insert(indent);
            if indent != ci {
                continue; // вложенные уровни не смотрим
            }
            if let Some(v) = t.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
                return is_true(v);
            }
        }
        false
    }

    /// Значение верхнеуровневого ключа (`external-controller`, `secret`, `mixed-port`).
    pub fn top_value(&self, key: &str) -> Option<String> {
        let prefix = format!("{key}:");
        self.lines.iter().find_map(|l| {
            let rest = l.strip_prefix(&prefix)?;
            let v = match rest.find(" #") {
                Some(i) => &rest[..i],
                None => rest,
            };
            Some(v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        })
    }
}

/// Покрывает ли какое-то из правил этот адрес.
pub fn covers(rules: &[Rule], host: &str) -> bool {
    let h = host.to_lowercase();
    rules.iter().any(|r| {
        let v = r.value.to_lowercase();
        match r.kind.as_str() {
            "DOMAIN-SUFFIX" => h == v || h.ends_with(&format!(".{v}")),
            "DOMAIN" => h == v,
            "DOMAIN-KEYWORD" => h.contains(&v),
            _ => false,
        }
    })
}

// ================================================================ проверка ввода

/// Приводит ввод пользователя к правилу. Понимает ссылки, `*.domain`, `.domain`, порты, пути.
pub fn normalize(kind: &str, raw: &str, action: &str) -> Result<Rule, String> {
    let kind = kind.trim().to_uppercase();
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!("неизвестный тип правила: {kind}"));
    }
    validate_action(action)?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("пустое значение".into());
    }
    let (value, extra) = match kind.as_str() {
        "PROCESS-NAME" => (normalize_process(raw)?, String::new()),
        "IP-CIDR" | "IP-CIDR6" => (normalize_cidr(raw)?, "no-resolve".to_string()),
        "DOMAIN-KEYWORD" => {
            let v = raw.to_lowercase();
            if v.chars().any(|c| c.is_whitespace() || c == ',' || c == '#' || c == '"' || c == '\'') {
                return Err("ключевое слово не должно содержать пробелы, запятые и кавычки".into());
            }
            (v, String::new())
        }
        _ => (normalize_domain(raw)?, String::new()),
    };
    let kind = if kind == "IP-CIDR" && value.contains(':') { "IP-CIDR6".to_string() } else { kind };
    Ok(Rule { kind, value, action: action.trim().to_string(), extra })
}

pub fn normalize_domain(raw: &str) -> Result<String, String> {
    let mut s = raw.trim().to_lowercase();
    if let Some(i) = s.find("://") {
        s = s[i + 3..].to_string();
    }
    // user:pass@host
    if let Some(i) = s.find('@') {
        if !s[..i].contains('/') {
            s = s[i + 1..].to_string();
        }
    }
    let end = s.find(['/', '?', '#']).unwrap_or(s.len());
    s.truncate(end);
    if let Some(i) = s.rfind(':') {
        if s[i + 1..].chars().all(|c| c.is_ascii_digit()) {
            s.truncate(i);
        }
    }
    let s = s.trim_start_matches("*.").trim_start_matches('.').trim_end_matches('.').to_string();
    if s.is_empty() {
        return Err("пустой домен".into());
    }
    if s.len() > 253 {
        return Err("слишком длинный домен".into());
    }
    if !s.contains('.') {
        return Err(format!("«{s}» не похоже на домен (нет точки)"));
    }
    for label in s.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("некорректный домен: {s}"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("часть домена не может начинаться или заканчиваться дефисом: {label}"));
        }
        if !label.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("недопустимые символы в домене: {s}"));
        }
    }
    Ok(s)
}

pub fn normalize_process(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_matches('"');
    // если вставили полный путь — берём имя файла
    let s = s.rsplit(['\\', '/']).next().unwrap_or(s).trim();
    if s.is_empty() {
        return Err("пустое имя процесса".into());
    }
    if s.chars().any(|c| c.is_control() || ",:*?\"<>|#".contains(c)) {
        return Err(format!("недопустимые символы в имени процесса: {s}"));
    }
    let mut s = s.to_string();
    if !s.to_lowercase().ends_with(".exe") {
        s.push_str(".exe");
    }
    Ok(s)
}

pub fn normalize_cidr(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let (ip, prefix) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    let ip: IpAddr = ip.parse().map_err(|_| format!("некорректный IP: {ip}"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let p: u32 = match prefix {
        Some(p) => p.parse().map_err(|_| format!("некорректная маска: /{p}"))?,
        None => max,
    };
    if p > max {
        return Err(format!("маска /{p} больше допустимой /{max}"));
    }
    Ok(format!("{ip}/{p}"))
}

pub fn validate_action(a: &str) -> Result<(), String> {
    let a = a.trim();
    if a.is_empty() {
        return Err("не выбрано действие".into());
    }
    if a.chars().any(|c| c == ',' || c == '\n' || c == '\r' || c == '#') {
        return Err("недопустимое имя группы".into());
    }
    Ok(())
}

/// Предлагаемый «корневой» домен для правила DOMAIN-SUFFIX: cdn.static.example.co.uk → example.co.uk.
pub fn suggest_suffix(host: &str) -> String {
    const SECOND_LEVEL: &[&str] = &[
        "co.uk", "org.uk", "ac.uk", "com.au", "net.au", "co.jp", "co.kr", "com.br", "com.cn", "com.tr",
        "com.ua", "co.il", "co.in", "com.mx", "com.ru", "msk.ru", "spb.ru", "net.ru", "org.ru", "pp.ua",
        "github.io", "vercel.app", "netlify.app", "pages.dev", "workers.dev", "herokuapp.com",
        "appspot.com", "cloudfront.net", "azurewebsites.net", "blogspot.com",
    ];
    let h = host.trim().trim_end_matches('.').to_lowercase();
    if h.parse::<IpAddr>().is_ok() {
        return h;
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() <= 2 {
        return h;
    }
    let last2 = labels[labels.len() - 2..].join(".");
    let take = if SECOND_LEVEL.contains(&last2.as_str()) { 3 } else { 2 };
    labels[labels.len().saturating_sub(take)..].join(".")
}

// ================================================================ импорт / экспорт

#[derive(Serialize, Debug)]
pub struct ImportLine {
    pub line: usize,
    pub source: String,
    pub rule: Option<Rule>,
    pub error: Option<String>,
}

/// Разбирает текст импорта. Понимает:
///   DOMAIN-SUFFIX,example.com,Proxy   — полное правило
///   - PROCESS-NAME,app.exe,DIRECT     — строку прямо из config.yaml
///   example.com / https://site/x      — домен (действие по умолчанию)
///   Telegram.exe                      — процесс
///   1.2.3.0/24                        — IP-сеть
/// Пустые строки и строки с # пропускаются.
pub fn parse_import(text: &str, default_action: &str) -> Vec<ImportLine> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let t = raw.trim().trim_start_matches('\u{feff}');
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        let t = t.strip_prefix("- ").unwrap_or(t).trim();
        let res = if t.contains(',') {
            match Rule::parse(&format!("- {t}")) {
                Some(r) => normalize(&r.kind, &r.value, &r.action),
                None => {
                    // «домен,действие» без типа
                    let mut p = t.splitn(2, ',');
                    let v = p.next().unwrap_or("");
                    let a = p.next().unwrap_or(default_action);
                    guess(v, a)
                }
            }
        } else {
            guess(t, default_action)
        };
        out.push(match res {
            Ok(r) => ImportLine { line: i + 1, source: raw.to_string(), rule: Some(r), error: None },
            Err(e) => ImportLine { line: i + 1, source: raw.to_string(), rule: None, error: Some(e) },
        });
    }
    out
}

fn guess(v: &str, action: &str) -> Result<Rule, String> {
    let v = v.trim();
    let lower = v.to_lowercase();
    if lower.ends_with(".exe") {
        normalize("PROCESS-NAME", v, action)
    } else if normalize_cidr(v).is_ok() {
        normalize("IP-CIDR", v, action)
    } else {
        normalize("DOMAIN-SUFFIX", v, action)
    }
}

pub fn export_text(rules: &[Rule]) -> String {
    let mut s = format!(
        "# Mihomo Tray — пользовательские правила\n# Экспорт: {}\n# Формат: ТИП,значение,действие\n\n",
        crate::util::timestamp(false)
    );
    for r in rules {
        s.push_str(r.to_line("").trim_start_matches("- "));
        s.push('\n');
    }
    s
}

// ================================================================ файл + перезагрузка

#[derive(Serialize, Clone, Debug)]
pub struct ApplyResult {
    /// Изменение сохранено.
    pub ok: bool,
    /// Mihomo уже применил изменение (false — служба выключена, применится при запуске).
    pub live: bool,
    pub message: String,
}

fn read_config() -> Result<(Vec<u8>, Doc), String> {
    let path = util::config_path();
    let bytes = fs::read(&path).map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?;
    let text = String::from_utf8(bytes.clone()).map_err(|_| "config.yaml не в кодировке UTF-8".to_string())?;
    Ok((bytes, Doc::parse(&text)))
}

pub fn load_doc() -> Result<Doc, String> {
    let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_config().map(|(_, d)| d)
}

fn backup(original: &[u8]) {
    let dir = util::backups_dir();
    let stamp = util::timestamp(true);
    let mut name = format!("config-{stamp}.yaml");
    for i in 2..100 {
        if !dir.join(&name).exists() {
            break;
        }
        name = format!("config-{stamp}_{i:02}.yaml");
    }
    if let Err(e) = fs::write(dir.join(&name), original) {
        crate::log_warn!("не удалось сделать бэкап конфига: {e}");
        return;
    }
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut files: Vec<_> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config-") && n.ends_with(".yaml"))
            .collect();
        files.sort();
        while files.len() > MAX_BACKUPS {
            let old = files.remove(0);
            let _ = fs::remove_file(dir.join(old));
        }
    }
}

/// Применяет изменение к config.yaml с бэкапом и откатом.
pub fn mutate<F>(api: &Api, what: &str, f: F) -> ApplyResult
where
    F: FnOnce(&mut Doc) -> Result<(), String>,
{
    let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (original, mut doc) = match read_config() {
        Ok(v) => v,
        Err(e) => return fail(what, e),
    };
    if let Err(e) = f(&mut doc) {
        return fail(what, e);
    }
    let new_text = doc.render();
    if new_text.as_bytes() == original.as_slice() {
        return ApplyResult { ok: true, live: true, message: "Изменений нет".into() };
    }
    write_and_reload(api, what, &original, new_text.as_bytes())
}

fn write_and_reload(api: &Api, what: &str, original: &[u8], new_bytes: &[u8]) -> ApplyResult {
    let path = util::config_path();
    backup(original);
    if let Err(e) = util::write_atomic(&path, new_bytes) {
        return fail(what, e);
    }
    match api.reload_config(&path) {
        Ok(()) => {
            crate::log_info!("{what}: применено");
            ApplyResult { ok: true, live: true, message: String::new() }
        }
        Err(ApiError::Unreachable(_)) => {
            crate::log_info!("{what}: сохранено, Mihomo не запущен — применится при старте");
            ApplyResult { ok: true, live: false, message: "Сохранено. Применится при включении VPN".into() }
        }
        Err(e) => {
            // Mihomo отклонил конфиг — возвращаем как было
            let restore = util::write_atomic(&path, original);
            let _ = api.reload_config(&path);
            let mut msg = format!("Mihomo отклонил конфигурацию, изменения отменены: {e}");
            if let Err(re) = restore {
                msg.push_str(&format!(". ВНИМАНИЕ: откат не удался ({re}), восстановите конфиг из бэкапа в настройках"));
            }
            crate::log_err!("{what}: {msg}");
            ApplyResult { ok: false, live: false, message: msg }
        }
    }
}

fn fail(what: &str, e: String) -> ApplyResult {
    crate::log_warn!("{what}: {e}");
    ApplyResult { ok: false, live: false, message: e }
}

// ---------------------------------------------------------------- бэкапы

#[derive(Serialize)]
pub struct BackupInfo {
    pub name: String,
    pub size: u64,
}

pub fn list_backups() -> Vec<BackupInfo> {
    let mut v: Vec<BackupInfo> = fs::read_dir(util::backups_dir())
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !name.starts_with("config-") {
                        return None;
                    }
                    Some(BackupInfo { size: e.metadata().map(|m| m.len()).unwrap_or(0), name })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.name.cmp(&a.name));
    v
}

pub fn restore_backup(api: &Api, name: &str) -> ApplyResult {
    if name.contains(['/', '\\']) || name.contains("..") || !name.starts_with("config-") {
        return fail("restore", "некорректное имя бэкапа".into());
    }
    let _g = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let data = match fs::read(util::backups_dir().join(name)) {
        Ok(d) => d,
        Err(e) => return fail("restore", format!("не удалось прочитать бэкап: {e}")),
    };
    let original = match fs::read(util::config_path()) {
        Ok(d) => d,
        Err(e) => return fail("restore", e.to_string()),
    };
    write_and_reload(api, &format!("восстановление {name}"), &original, &data)
}

// ================================================================ тесты

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "mode: rule\r\nexternal-controller: \"127.0.0.1:9090\"\r\nproxy-groups:\r\n  - name: \"Proxy\"\r\n    type: select\r\n  - name: Auto\r\nrules:\r\n  # ==CUSTOM_PROCESS_START==\r\n  - PROCESS-NAME,VoiceStudio.exe,Proxy\r\n  # ==CUSTOM_PROCESS_END==\r\n  # ==CUSTOM_DOMAIN_START==\r\n  - DOMAIN-SUFFIX,tiktok.com,Proxy\r\n\r\n  - DOMAIN-SUFFIX,7tv.app,Proxy\r\n  # ==CUSTOM_DOMAIN_END==\r\n  - MATCH,Proxy\r\n";

    #[test]
    fn roundtrip_keeps_format() {
        let d = Doc::parse(SAMPLE);
        assert_eq!(d.render(), SAMPLE);
        assert_eq!(d.rules().len(), 3);
        assert_eq!(d.first_group().as_deref(), Some("Proxy"));
        assert_eq!(d.top_value("external-controller").as_deref(), Some("127.0.0.1:9090"));
    }

    #[test]
    fn add_remove_shift_replace() {
        let mut d = Doc::parse(SAMPLE);
        let r = normalize("DOMAIN-SUFFIX", "https://www.Example.com/path?x=1", "DIRECT").unwrap();
        assert_eq!(r.value, "www.example.com");
        assert!(d.add(&r).unwrap());
        assert!(!d.add(&r).unwrap(), "дубликат не добавляется");
        let rules = d.rules();
        assert_eq!(rules.last().unwrap().action, "DIRECT");
        // добавилось перед маркером конца, внутри блока
        let txt = d.render();
        assert!(txt.contains("  - DOMAIN-SUFFIX,www.example.com,DIRECT\r\n  # ==CUSTOM_DOMAIN_END=="));

        d.shift(&r.key(), -1).unwrap();
        assert_eq!(d.rules()[2].value, "www.example.com");
        d.shift("DOMAIN-SUFFIX,tiktok.com", -1).unwrap(); // уже первый — без изменений
        assert_eq!(d.rules()[1].value, "tiktok.com");

        let mut r2 = r.clone();
        r2.action = "REJECT".into();
        d.replace(&r.key(), &r2).unwrap();
        assert!(d.rules().iter().any(|x| x.value == "www.example.com" && x.action == "REJECT"));

        assert!(d.remove(&r.key()));
        assert!(!d.remove(&r.key()));
        assert_eq!(d.render(), SAMPLE);
    }

    #[test]
    fn process_goes_to_process_block() {
        let mut d = Doc::parse(SAMPLE);
        let r = normalize("PROCESS-NAME", r"C:\Program Files\Telegram\Telegram.exe", "Proxy").unwrap();
        assert_eq!(r.value, "Telegram.exe");
        d.add(&r).unwrap();
        let txt = d.render();
        assert!(txt.contains("Telegram.exe,Proxy\r\n  # ==CUSTOM_PROCESS_END=="));
    }

    #[test]
    fn probe_group_and_rules() {
        let src = "proxy-providers:\n  my-subscription:\n    type: http\n    url: \"x\"\n  other:\n    type: file\nproxy-groups:\n  - name: \"Proxy\"\n    type: select\nrules:\n  - MATCH,DIRECT\n";
        let mut d = Doc::parse(src);
        assert_eq!(d.provider_names(), vec!["my-subscription", "other"]);
        assert!(d.ensure_probe().unwrap());
        assert!(!d.ensure_probe().unwrap(), "второй раз ничего не добавляет");
        let t = d.render();
        assert!(t.contains("  - name: \"TrayProbe\"\n    type: select\n    hidden: true\n    use:\n      - my-subscription\n      - other\n    proxies:\n      - DIRECT\n  - name: \"Proxy\""), "{t}");
        assert!(t.contains("rules:\n  # служебное правило Mihomo Tray: тест скорости нод\n  - DOMAIN,cachefly.cachefly.net,TrayProbe\n  - DOMAIN,speedtest.tele2.net,TrayProbe\n  - MATCH,DIRECT"), "{t}");
        assert_eq!(d.first_group().as_deref(), Some("Proxy"), "служебная группа не должна стать главной");
        assert!(d.remove_probe_group());
        assert!(!d.render().contains("TrayProbe\"\n"), "группа удалена");
        assert!(d.render().contains("  - name: \"Proxy\""));
        let p = Doc::parse("proxies:\n  - name: \"NL 1\"\n    type: vless\n  - {name: DE, type: ss}\n  # - name: X\nproxy-groups:\n");
        assert_eq!(p.static_proxy_names(), vec!["NL 1", "DE"]);
    }

    #[test]
    fn section_flags() {
        let d = Doc::parse("mode: rule\ntun:\n  # комментарий\n  enable: true  # да\n  dns-hijack:\n    - any:53\nsniffer:\n  enable: false\ndns:\n  nested:\n    enable: true\n");
        assert!(d.section_flag("tun", "enable"));
        assert!(!d.section_flag("sniffer", "enable"));
        assert!(!d.section_flag("dns", "enable"), "вложенный enable не считается");
        assert!(!d.section_flag("nope", "enable"));
        assert!(Doc::parse("tun: {enable: true, stack: mixed}\n").section_flag("tun", "enable"));
        assert!(!Doc::parse("tun: {enable: false}\n").section_flag("tun", "enable"));
        assert!(!Doc::parse("tun:\nmode: rule\n").section_flag("tun", "enable"));
    }

    #[test]
    fn creates_blocks_if_missing() {
        let mut d = Doc::parse("rules:\n  - MATCH,Proxy\n");
        d.add(&normalize("DOMAIN-SUFFIX", "a.com", "Proxy").unwrap()).unwrap();
        let t = d.render();
        assert!(t.starts_with("rules:\n  # ==CUSTOM_PROCESS_START==\n  # ==CUSTOM_PROCESS_END==\n  # ==CUSTOM_DOMAIN_START==\n  - DOMAIN-SUFFIX,a.com,Proxy\n  # ==CUSTOM_DOMAIN_END==\n  - MATCH,Proxy"));
    }

    #[test]
    fn validation() {
        assert!(normalize_domain("a.com,DIRECT").is_err());
        assert!(normalize_domain("bad domain.com").is_err());
        assert!(normalize_domain("localhost").is_err());
        assert_eq!(normalize_domain("*.youtube.com").unwrap(), "youtube.com");
        assert_eq!(normalize_domain("http://user@site.ru:8080/x").unwrap(), "site.ru");
        assert_eq!(normalize_domain("пример.рф").unwrap(), "пример.рф");
        assert_eq!(normalize_cidr("1.2.3.4").unwrap(), "1.2.3.4/32");
        assert!(normalize_cidr("1.2.3.4/40").is_err());
        assert_eq!(normalize("IP-CIDR", "2001:db8::/32", "DIRECT").unwrap().kind, "IP-CIDR6");
        assert!(normalize_process("bad|name").is_err());
        assert!(validate_action("Proxy,DIRECT").is_err());
        assert!(normalize("FOO", "x", "Proxy").is_err());
    }

    #[test]
    fn suffix() {
        assert_eq!(suggest_suffix("rr3---sn.googlevideo.com"), "googlevideo.com");
        assert_eq!(suggest_suffix("a.b.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(suggest_suffix("example.com"), "example.com");
        assert_eq!(suggest_suffix("1.2.3.4"), "1.2.3.4");
    }

    #[test]
    fn parse_variants() {
        let r = Rule::parse("  - DOMAIN-SUFFIX, x.com , Proxy  # comment").unwrap();
        assert_eq!((r.value.as_str(), r.action.as_str()), ("x.com", "Proxy"));
        let r = Rule::parse("- IP-CIDR,1.1.1.1/32,DIRECT,no-resolve").unwrap();
        assert_eq!(r.extra, "no-resolve");
        assert!(Rule::parse("  # comment").is_none());
        assert!(Rule::parse("").is_none());
    }

    #[test]
    fn import_export() {
        let text = "# comment\nDOMAIN-SUFFIX,a.com,DIRECT\n- PROCESS-NAME,tg.exe,Proxy\nb.org\nhttps://c.net/path\nApp.exe\n10.0.0.0/8\nbad domain\nd.com,REJECT\n";
        let v = parse_import(text, "Proxy");
        let ok: Vec<_> = v.iter().filter_map(|l| l.rule.clone()).collect();
        assert_eq!(ok.len(), 7);
        assert_eq!(v.iter().filter(|l| l.error.is_some()).count(), 1);
        assert_eq!(ok[0], Rule { kind: "DOMAIN-SUFFIX".into(), value: "a.com".into(), action: "DIRECT".into(), extra: "".into() });
        assert_eq!(ok[2].action, "Proxy");
        assert_eq!(ok[4].kind, "PROCESS-NAME");
        assert_eq!(ok[5].kind, "IP-CIDR");
        assert_eq!(ok[6].action, "REJECT");
        let exp = export_text(&ok);
        let back: Vec<_> = parse_import(&exp, "X").into_iter().filter_map(|l| l.rule).collect();
        assert_eq!(back, ok);
    }

    #[test]
    fn real_config_if_present() {
        let p = std::path::Path::new("/mnt/user-data/uploads/mihomo/config.yaml");
        if let Ok(t) = std::fs::read_to_string(p) {
            let d = Doc::parse(&t);
            assert_eq!(d.render(), t, "рендер должен совпадать байт-в-байт");
            assert!(d.rules().len() >= 2);
            assert_eq!(d.first_group().as_deref(), Some("Proxy"));
        }
    }
}

/// Интеграционный тест с живым Mihomo: MIHOMO_DIR=<папка> cargo test live -- --ignored
#[cfg(test)]
mod live {
    use super::*;
    use crate::api::Api;

    #[test]
    #[ignore]
    fn live_mihomo() {
        let api = Api::from_config();
        assert!(api.version().is_ok(), "Mihomo должен быть запущен");
        let before = fs::read_to_string(util::config_path()).unwrap();

        // добавление + горячая перезагрузка
        let r = normalize("DOMAIN-SUFFIX", "example.org", "DIRECT").unwrap();
        let res = mutate(&api, "test add", |d| d.add(&r).map(|_| ()));
        assert!(res.ok && res.live, "{res:?}");
        let rules = api.get("/rules").unwrap();
        let s = rules.to_string();
        assert!(s.contains("example.org"), "Mihomo должен видеть новое правило: {s}");

        // правило с несуществующей группой — Mihomo отклонит, файл откатится
        let bad = Rule { kind: "DOMAIN-SUFFIX".into(), value: "bad.org".into(), action: "NoSuchGroup".into(), extra: String::new() };
        let snapshot = fs::read_to_string(util::config_path()).unwrap();
        let res = mutate(&api, "test bad", |d| d.add(&bad).map(|_| ()));
        assert!(!res.ok, "должен быть отказ: {res:?}");
        assert_eq!(fs::read_to_string(util::config_path()).unwrap(), snapshot, "файл должен откатиться");
        assert!(!api.get("/rules").unwrap().to_string().contains("bad.org"));

        // режим, группы, выбор ноды
        api.set_mode("global").unwrap();
        assert_eq!(api.configs().unwrap()["mode"], "global");
        api.set_mode("rule").unwrap();
        api.select("Proxy", "DIRECT").unwrap();
        let p = api.proxies().unwrap();
        assert_eq!(crate::api::resolve_chain(&p, "Proxy"), vec!["Proxy", "DIRECT"]);
        api.select("Proxy", "Auto").unwrap();
        let p = api.proxies().unwrap();
        assert_eq!(crate::api::resolve_chain(&p, "Proxy"), vec!["Proxy", "Auto", "NL-1"]);
        assert_eq!(api.delay("NL-1", crate::api::DEFAULT_TEST_URL).unwrap(), Some(0), "мёртвая нода → Some(0)");
        // ноды из proxy-provider должны быть видны вместе с историей пинга
        api.group_delay("Sub", crate::api::DEFAULT_TEST_URL).unwrap();
        let p = api.proxies().unwrap();
        assert_eq!(p["SE-1"]["type"], "Socks5", "нода подписки должна подмешиваться");
        assert_eq!(crate::api::last_delay(&p["SE-1"]), Some(0));
        assert!(api.connections().unwrap().get("uploadTotal").is_some());

        // удаление и бэкапы
        let res = mutate(&api, "test del", |d| { d.remove(&r.key()); Ok(()) });
        assert!(res.ok && res.live);
        assert_eq!(fs::read_to_string(util::config_path()).unwrap(), before);
        assert!(list_backups().len() >= 2);
        let first = list_backups().last().unwrap().name.clone();
        assert!(restore_backup(&api, &first).ok);
        assert!(restore_backup(&api, "../evil").ok == false);
        // вернуть тестовый конфиг в исходное состояние
        fs::write(util::config_path(), &before).unwrap();
        api.reload_config(&util::config_path()).unwrap();
    }
}
