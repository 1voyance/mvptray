//! Клиент REST API Mihomo (external-controller).

use crate::util;
use serde_json::{json, Value};
use std::fmt;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum ApiError {
    /// Mihomo не отвечает (служба остановлена или API выключен).
    Unreachable(String),
    /// Mihomo ответил ошибкой.
    Http(u16, String),
    Other(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(m) => write!(f, "Mihomo не отвечает ({m})"),
            ApiError::Http(c, m) => write!(f, "HTTP {c}: {m}"),
            ApiError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl From<ApiError> for String {
    fn from(e: ApiError) -> String {
        e.to_string()
    }
}

#[derive(Clone)]
pub struct Api {
    base: String,
    secret: Option<String>,
    agent: ureq::Agent,
}

impl Api {
    /// Адрес и секрет берутся из config.yaml (external-controller / secret).
    pub fn from_config() -> Api {
        let doc = crate::rules::load_doc().ok();
        let ctrl = doc
            .as_ref()
            .and_then(|d| d.top_value("external-controller"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "127.0.0.1:9090".into());
        let ctrl = ctrl.replace("0.0.0.0", "127.0.0.1");
        let ctrl = if let Some(port) = ctrl.strip_prefix(':') { format!("127.0.0.1:{port}") } else { ctrl };
        let secret = doc.and_then(|d| d.top_value("secret")).filter(|s| !s.is_empty());
        Api::new(&format!("http://{ctrl}"), secret)
    }

    pub fn new(base: &str, secret: Option<String>) -> Api {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(800))
            .timeout(Duration::from_secs(4))
            .build();
        Api { base: base.trim_end_matches('/').to_string(), secret, agent }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn req(&self, method: &str, path: &str) -> ureq::Request {
        let r = self.agent.request(method, &format!("{}{}", self.base, path));
        match &self.secret {
            Some(s) => r.set("Authorization", &format!("Bearer {s}")),
            None => r,
        }
    }

    fn handle(res: Result<ureq::Response, ureq::Error>) -> Result<Option<Value>, ApiError> {
        match res {
            Ok(r) => {
                if r.status() == 204 {
                    return Ok(None);
                }
                let s = r.into_string().map_err(|e| ApiError::Other(e.to_string()))?;
                if s.trim().is_empty() {
                    return Ok(None);
                }
                serde_json::from_str(&s).map(Some).map_err(|e| ApiError::Other(format!("плохой JSON: {e}")))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("message").and_then(Value::as_str).map(String::from))
                    .unwrap_or(body);
                Err(ApiError::Http(code, msg))
            }
            Err(ureq::Error::Transport(t)) => Err(ApiError::Unreachable(t.kind().to_string())),
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, ApiError> {
        Self::handle(self.req("GET", path).call()).map(|v| v.unwrap_or(Value::Null))
    }

    pub fn send(&self, method: &str, path: &str, body: Value) -> Result<(), ApiError> {
        Self::handle(self.req(method, path).send_json(body)).map(|_| ())
    }

    // ------------------------------------------------------------ методы

    pub fn version(&self) -> Result<String, ApiError> {
        Ok(self.get("/version")?.get("version").and_then(Value::as_str).unwrap_or("").to_string())
    }

    pub fn configs(&self) -> Result<Value, ApiError> {
        self.get("/configs")
    }

    pub fn set_mode(&self, mode: &str) -> Result<(), ApiError> {
        self.send("PATCH", "/configs", json!({ "mode": mode }))
    }

    pub fn reload_config(&self, path: &Path) -> Result<(), ApiError> {
        let p = path.to_string_lossy().replace('\\', "/");
        self.send("PUT", "/configs?force=true", json!({ "path": p }))
    }

    /// Все прокси и группы. Ноды из proxy-providers (подписки) Mihomo отдаёт
    /// отдельно в /providers/proxies — подмешиваем их, иначе у нод нет пинга и типа.
    pub fn proxies(&self) -> Result<Value, ApiError> {
        let mut map = self.get("/proxies")?.get("proxies").cloned().unwrap_or(Value::Null);
        if let (Some(obj), Ok(Value::Object(provs))) = (map.as_object_mut(), self.proxy_providers()) {
            for (pname, p) in provs {
                for node in p.get("proxies").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(name) = node.get("name").and_then(Value::as_str) {
                        if !obj.contains_key(name) {
                            let mut n = node.clone();
                            if let Some(o) = n.as_object_mut() {
                                o.insert("_provider".into(), Value::String(pname.clone()));
                            }
                            obj.insert(name.to_string(), n);
                        }
                    }
                }
            }
        }
        Ok(map)
    }

    pub fn select(&self, group: &str, node: &str) -> Result<(), ApiError> {
        self.send("PUT", &format!("/proxies/{}", util::url_encode(group)), json!({ "name": node }))
    }

    /// Долгий запрос (проверка пинга): Mihomo отвечает только после таймаута теста.
    fn get_slow(&self, path: &str) -> Result<Value, ApiError> {
        Self::handle(self.req("GET", path).timeout(Duration::from_secs(25)).call()).map(|v| v.unwrap_or(Value::Null))
    }

    /// Пинг ноды. Ok(Some(0)) — нода не ответила; Err — проверить не удалось (это не значит, что нода мертва).
    pub fn delay(&self, node: &str, url: &str) -> Result<Option<u32>, ApiError> {
        let path = format!("/proxies/{}/delay?timeout={DELAY_TIMEOUT_MS}&url={}", util::url_encode(node), util::url_encode(url));
        match self.get_slow(&path) {
            Ok(v) => Ok(v.get("delay").and_then(Value::as_u64).map(|d| d as u32)),
            Err(ApiError::Http(504, _)) | Err(ApiError::Http(503, _)) | Err(ApiError::Http(408, _)) => Ok(Some(0)),
            Err(e) => Err(e),
        }
    }

    /// Пинг всех нод группы (Mihomo сам обновит историю задержек).
    /// «все ноды не ответили» — это не ошибка запроса, а результат.
    pub fn group_delay(&self, group: &str, url: &str) -> Result<(), ApiError> {
        let path = format!("/group/{}/delay?timeout={DELAY_TIMEOUT_MS}&url={}", util::url_encode(group), util::url_encode(url));
        match self.get_slow(&path) {
            Ok(_) => Ok(()),
            Err(ApiError::Http(504, _)) | Err(ApiError::Http(503, _)) | Err(ApiError::Http(408, _)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn connections(&self) -> Result<Value, ApiError> {
        self.get("/connections")
    }

    pub fn proxy_providers(&self) -> Result<Value, ApiError> {
        Ok(self.get("/providers/proxies")?.get("providers").cloned().unwrap_or(Value::Null))
    }

    /// Пинг одной ноды для умного выбора. Ok(Some(0)) — не ответила, Err — проверить не удалось.
    /// Ноды подписки при необходимости проверяются через провайдера.
    pub fn probe_delay(&self, node: &str, provider: Option<&str>, url: &str, timeout_ms: u32) -> Result<Option<u32>, ApiError> {
        let q = format!("timeout={timeout_ms}&url={}", util::url_encode(url));
        let direct = format!("/proxies/{}/delay?{q}", util::url_encode(node));
        let res = match self.get_slow(&direct) {
            Err(ApiError::Http(404, _)) if provider.is_some() => {
                let p = format!(
                    "/providers/proxies/{}/{}/healthcheck?{q}",
                    util::url_encode(provider.unwrap_or_default()),
                    util::url_encode(node)
                );
                self.get_slow(&p)
            }
            r => r,
        };
        match res {
            Ok(v) => Ok(v.get("delay").and_then(Value::as_u64).map(|d| d as u32)),
            Err(ApiError::Http(504, _)) | Err(ApiError::Http(503, _)) | Err(ApiError::Http(408, _)) => Ok(Some(0)),
            Err(e) => Err(e),
        }
    }

    /// Долгие POST/PUT (обновление баз, правил, панели — Mihomo скачивает файлы).
    pub fn post_slow(&self, path: &str) -> Result<(), ApiError> {
        Self::handle(self.req("POST", path).timeout(Duration::from_secs(180)).send_json(json!({}))).map(|_| ())
    }

    pub fn put_slow(&self, path: &str) -> Result<(), ApiError> {
        Self::handle(self.req("PUT", path).timeout(Duration::from_secs(120)).send_json(json!({}))).map(|_| ())
    }

    /// Снять ручную фиксацию ноды в url-test/fallback группе (вернуть родной автовыбор Mihomo).
    pub fn unfix(&self, group: &str) -> Result<(), ApiError> {
        Self::handle(self.req("DELETE", &format!("/proxies/{}", util::url_encode(group))).call()).map(|_| ())
    }

}

pub const DEFAULT_TEST_URL: &str = "http://www.gstatic.com/generate_204";
const DELAY_TIMEOUT_MS: u32 = 5000;

/// URL проверки для группы: её testUrl из конфига, иначе стандартный.
pub fn test_url(proxies: &Value, group: &str) -> String {
    proxies
        .get(group)
        .and_then(|g| g.get("testUrl"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TEST_URL)
        .to_string()
}

/// Последняя измеренная задержка прокси. Some(0) — нода не ответила.
/// Mihomo хранит историю и в `history`, и по каждому URL проверки в `extra[url].history`;
/// берём самую свежую запись из всех.
pub fn last_delay(proxy: &Value) -> Option<u32> {
    let mut best: Option<(String, u32)> = None;
    let mut consider = |hist: Option<&Value>| {
        if let Some(last) = hist.and_then(Value::as_array).and_then(|a| a.last()) {
            let t = last.get("time").and_then(Value::as_str).unwrap_or("").to_string();
            if let Some(d) = last.get("delay").and_then(Value::as_u64) {
                if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    best = Some((t, d as u32));
                }
            }
        }
    };
    consider(proxy.get("history"));
    if let Some(extra) = proxy.get("extra").and_then(Value::as_object) {
        for v in extra.values() {
            consider(v.get("history"));
        }
    }
    best.map(|(_, d)| d)
}

/// Цепочка выбора: Proxy → Auto → Subscription → «Нидерланды 1».
pub fn resolve_chain(proxies: &Value, start: &str) -> Vec<String> {
    let mut chain = vec![start.to_string()];
    let mut cur = start.to_string();
    for _ in 0..10 {
        let next = proxies.get(&cur).and_then(|p| p.get("now")).and_then(Value::as_str).unwrap_or("");
        if next.is_empty() || chain.iter().any(|c| c == next) {
            break;
        }
        chain.push(next.to_string());
        cur = next.to_string();
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chain() {
        let p = json!({
            "Proxy": {"now": "Auto"},
            "Auto": {"now": "NL 1"},
            "NL 1": {"history": [{"delay": 10}, {"delay": 48}]}
        });
        assert_eq!(resolve_chain(&p, "Proxy"), vec!["Proxy", "Auto", "NL 1"]);
        assert_eq!(last_delay(&p["NL 1"]), Some(48));
        assert_eq!(last_delay(&p["Auto"]), None);
        let n = json!({"history": [], "extra": {
            "http://a": {"history": [{"time": "2026-11-12T10:00:00+03:00", "delay": 0}]},
            "http://b": {"history": [{"time": "2026-11-12T10:05:00+03:00", "delay": 81}]}}});
        assert_eq!(last_delay(&n), Some(81));
    }
}
