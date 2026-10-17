//! F5 translation: OpenAI-compatible presets, DeepL, Edge as free fallback.

use pinshot_core::config::TranslateSettings;
use md5::Digest;
use pinshot_core::translate::{
    deepl_form_body, deepl_read_text, deepl_url, effective_target, lang_family, openai_read_content,
    openai_request_body, read_translation_ex, redact_secret, request_body, request_url,
    resolved_openai_model, resolved_openai_url, target_from_ui_lang,
};

pub fn default_target() -> String {
    let cfg = pinshot_core::config::load().unwrap_or_default();
    target_for(&cfg.translate, &cfg.language)
}

pub fn target_for(settings: &TranslateSettings, ui_lang: &str) -> String {
    let t = settings.target.trim();
    if t.is_empty() {
        target_from_ui_lang(ui_lang).to_string()
    } else {
        t.to_string()
    }
}

pub fn translate(text: &str, target: &str) -> Result<String, String> {
    let cfg = pinshot_core::config::load().unwrap_or_default();
    let mut settings = cfg.translate;
    if settings.target.trim().is_empty() {
        settings.target = target.to_string();
    }
    translate_with(&settings, &cfg.language, text)
}

pub fn translate_with(
    settings: &TranslateSettings,
    ui_lang: &str,
    text: &str,
) -> Result<String, String> {
    translate_with_opts(settings, ui_lang, text, true)
}

pub fn list_models(settings: &TranslateSettings) -> Result<Vec<String>, String> {
    let base = if settings.api_url.trim().is_empty() {
        pinshot_core::translate::service_preset(settings.normalized_provider())
            .map(|p| p.default_url).unwrap_or(pinshot_core::translate::OPENAI_URL_DEFAULT)
    } else { settings.api_url.trim() }.trim_end_matches('/');
    let url = if base.is_empty() { "https://api.deepseek.com/models".to_string() }
        else if base.ends_with("/models") { base.to_string() }
        else { format!("{base}/models") };
    if settings.api_key.trim().is_empty() { return Err("API key required".into()); }
    let agent = http_agent(settings)?;
    let raw = agent.get(&url).set("Authorization", &format!("Bearer {}", settings.api_key.trim()))
        .timeout(std::time::Duration::from_secs(15)).call()
        .map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let models = v.get("data").and_then(|x| x.as_array()).ok_or_else(|| "No models returned".to_string())?
        .iter().filter_map(|x| x.get("id").and_then(|x| x.as_str()).map(str::to_owned)).collect::<Vec<_>>();
    if models.is_empty() { Err("No models returned".into()) } else { Ok(models) }
}

fn http_agent(settings: &TranslateSettings) -> Result<ureq::Agent, String> {
    if settings.proxy.trim().is_empty() { return Ok(ureq::AgentBuilder::new().build()); }
    let proxy = ureq::Proxy::new(settings.proxy.trim()).map_err(|e| format!("Invalid provider proxy: {e}"))?;
    Ok(ureq::AgentBuilder::new().proxy(proxy).build())
}

/// `fallback`: F5 retries the free Edge endpoint if the configured API fails.
/// The settings Test button passes `false` so the real error is visible.
pub fn translate_with_opts(
    settings: &TranslateSettings,
    ui_lang: &str,
    text: &str,
    fallback: bool,
) -> Result<String, String> {
    let configured = target_for(settings, ui_lang);
    let started = std::time::Instant::now();
    let to = effective_target(text, &configured);
    let primary = match settings.normalized_provider() {
        "edge" => edge_translate(text, &to),
        "deepl" => deepl_translate(settings, text, &to),
        "baidu" => baidu_translate(settings, text, &to),
        _ => openai_translate(settings, text, &to),
    };
    match primary {
        Ok(s) => {
            crate::logutil::log("translate: request succeeded");
            crate::logutil::ms("translate: request", started);
            Ok(s)
        },
        Err(e) if fallback && settings.normalized_provider() != "edge" => {
            // Provider errors may echo credentials or source text. Keep them
            // for the UI result, but never persist the raw response.
            crate::logutil::error("translate: primary request failed; trying Edge fallback");
            match edge_translate(text, &to) {
                Ok(s) => {
                    crate::logutil::log("translate: Edge fallback succeeded");
                    crate::logutil::ms("translate: request with fallback", started);
                    Ok(s)
                },
                Err(_) => {
                    crate::logutil::error("translate: Edge fallback failed");
                    Err(e)
                },
            }
        }
        Err(e) => {
            crate::logutil::error("translate: request failed");
            crate::logutil::ms("translate: failed request", started);
            Err(e)
        },
    }
}

fn log_translate(provider: &str, n: usize, target: &str) {
    crate::logutil::log(&format!("translate {n} chars -> {target} via {provider}"));
}

fn edge_translate(text: &str, target: &str) -> Result<String, String> {
    let mut to = target.to_string();
    let out = post_edge(text, &to)?;
    if let Some(det) = out.detected.as_deref() {
        if lang_family(det) == lang_family(&to) {
            let retry = if lang_family(&to) == "en" {
                "zh-Hans"
            } else {
                "en"
            };
            crate::logutil::log(&format!(
                "translate detected {det} already {to}; retry {retry}"
            ));
            to = retry.into();
            return Ok(post_edge(text, &to)?.text);
        }
    }
    Ok(out.text)
}

fn post_edge(text: &str, target: &str) -> Result<pinshot_core::translate::Translation, String> {
    let body = request_body(text)?;
    let url = request_url(target);
    log_translate("edge", text.chars().count(), target);
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/128.0.0.0 Safari/537.36 Edg/128.0.0.0",
        )
        .timeout(std::time::Duration::from_secs(20))
        .send_string(&body)
        .map_err(|e| e.to_string())?;
    let raw = resp.into_string().map_err(|e| e.to_string())?;
    read_translation_ex(&raw)
}

fn openai_translate(
    settings: &TranslateSettings,
    text: &str,
    target: &str,
) -> Result<String, String> {
    let url = resolved_openai_url(settings);
    let body = openai_request_body(&resolved_openai_model(settings), text, target)?;
    log_translate(settings.normalized_provider(), text.chars().count(), target);
    let key = settings.api_key.trim();
    if key.is_empty() {
        return Err("OpenAI API key required".into());
    }
    if key.starts_with("http://") || key.starts_with("https://") {
        return Err("Please enter the OpenAI API key itself, not the API key webpage URL".into());
    }
    let agent = http_agent(settings)?;
    let mut req = agent.post(&url)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(30));
    if !key.is_empty() {
        req = req.set(
            "Authorization",
            &format!("Bearer {key}"),
        );
    }
    let raw = match req.send_string(&body) {
        Ok(resp) => resp.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, resp)) => {
            let err_body = resp.into_string().unwrap_or_default();
            let msg = openai_read_content(&err_body).unwrap_or_else(|_| {
                serde_json::from_str::<serde_json::Value>(&err_body)
                    .ok()
                    .and_then(|v| v.pointer("/error/message").and_then(|x| x.as_str()).map(str::to_owned))
                    .unwrap_or_else(|| if code == 401 { "Invalid OpenAI API key".into() } else { format!("HTTP {code}") })
            });
            return Err(redact_secret(&msg, &settings.api_key));
        }
        Err(e) => return Err(redact_secret(&e.to_string(), &settings.api_key)),
    };
    openai_read_content(&raw).map_err(|e| redact_secret(&e, &settings.api_key))
}

fn deepl_translate(
    settings: &TranslateSettings,
    text: &str,
    target: &str,
) -> Result<String, String> {
    let form = deepl_form_body(text, target)?;
    if settings.api_key.trim().is_empty() {
        return Err("DeepL API key required".into());
    }
    let key = settings.api_key.trim();
    // DeepL Free keys end in `:fx`; Pro keys do not. If the user left the
    // default URL in place, select the matching API host automatically.
    let configured_url = settings.api_url.trim();
    let url = if configured_url.is_empty()
        || configured_url == pinshot_core::translate::DEEPL_URL_DEFAULT
    {
        if key.ends_with(":fx") {
            "https://api-free.deepl.com/v2/translate".to_string()
        } else {
            "https://api.deepl.com/v2/translate".to_string()
        }
    } else {
        deepl_url(configured_url)
    };
    log_translate("deepl", text.chars().count(), target);
    let agent = http_agent(settings)?;
    let raw = match agent.post(&url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set(
            "Authorization",
            &format!("DeepL-Auth-Key {key}"),
        )
        .timeout(std::time::Duration::from_secs(20))
        .send_string(&form)
    {
        Ok(resp) => resp.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, resp)) => {
            let err_body = resp.into_string().unwrap_or_default();
            let msg = if code == 403 || code == 401 {
                serde_json::from_str::<serde_json::Value>(&err_body)
                    .ok().and_then(|v| v.get("message").and_then(|x| x.as_str()).map(str::to_owned))
                    .unwrap_or_else(|| "Invalid API key or wrong Free/Pro endpoint".into())
            } else if err_body.is_empty() { format!("HTTP {code}") } else { err_body };
            return Err(redact_secret(&msg, &settings.api_key));
        }
        Err(e) => return Err(redact_secret(&e.to_string(), &settings.api_key)),
    };
    Ok(deepl_read_text(&raw)?.text)
}

fn baidu_translate(settings: &TranslateSettings, text: &str, target: &str) -> Result<String, String> {
    if settings.app_id.trim().is_empty() || settings.api_key.trim().is_empty() {
        return Err("Baidu App ID and API key required".into());
    }
    let salt = format!("{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis());
    let from = "auto";
    let to = match target {
        "zh-Hans" => "zh",
        "zh-Hant" => "cht",
        "ja" | "ja-JP" => "jp",
        other => other,
    };
    let sign = baidu_sign(settings.app_id.trim(), text, &salt, settings.api_key.trim());
    let body = format!("q={}&from={}&to={}&appid={}&salt={}&sign={}",
        form_escape(text), from, form_escape(to), form_escape(settings.app_id.trim()), salt, sign);
    let configured = settings.api_url.trim();
    let base = if configured.is_empty() || configured.contains("qianfan.baidubce.com") {
        "https://fanyi-api.baidu.com/api/trans/vip/translate".to_string()
    } else {
        configured.trim_end_matches('/').to_string()
    };
    let url = if base.contains("/api/trans/vip/translate") { base } else { format!("{base}/api/trans/vip/translate") };
    log_translate("baidu", text.chars().count(), to);
    let agent = http_agent(settings)?;
    let raw = agent.post(&url).set("Content-Type", "application/x-www-form-urlencoded")
        .timeout(std::time::Duration::from_secs(20)).send_string(&body)
        .map_err(|e| redact_secret(&e.to_string(), &settings.api_key))?
        .into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if let Some(code) = v.get("error_code").and_then(|v| v.as_str()) {
        let msg = v.get("error_msg").and_then(|v| v.as_str()).unwrap_or(code);
        return Err(redact_secret(&format!("Baidu {code}: {msg}"), &settings.api_key));
    }
    let out = v.get("trans_result").and_then(|v| v.as_array())
        .ok_or_else(|| "Baidu returned no translation".to_string())?;
    let text = out.iter().filter_map(|x| x.get("dst").and_then(|v| v.as_str())).collect::<Vec<_>>().join("\n");
    if text.is_empty() { Err("Baidu returned no translation".into()) } else { Ok(text) }
}

fn baidu_sign(app_id: &str, query: &str, salt: &str, secret_key: &str) -> String {
    let sign_input = format!("{app_id}{query}{salt}{secret_key}");
    let digest = md5::Md5::digest(sign_input.as_bytes());
    format!("{digest:x}")
}

fn form_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') { out.push(*b as char); }
        else if *b == b' ' { out.push('+'); }
        else { out.push_str(&format!("%{b:02X}")); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinshot_core::config::TranslateSettings;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn spawn_mock(status: u16, body: &str) -> (String, Arc<Mutex<String>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let cap = captured.clone();
        let body = body.to_string();
        let h = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            for _ in 0..32 {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
                if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = &buf[..head_end];
                    let hs = String::from_utf8_lossy(headers);
                    let cl = hs
                        .lines()
                        .find_map(|l| {
                            l.split_once(':').and_then(|(k, v)| {
                                if k.eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + cl {
                        break;
                    }
                }
            }
            *cap.lock().unwrap() = String::from_utf8_lossy(&buf).into_owned();
            let reason = if status == 200 { "OK" } else { "ERR" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        (format!("http://{addr}/v1/chat/completions"), captured, h)
    }

    #[test]
    fn openai_compatible_mock_translates_and_sends_bearer() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"Hello from mock"}}]}"#;
        let (url, seen, h) = spawn_mock(200, json);
        let settings = TranslateSettings {
            provider: "openai".into(),
            api_url: url,
            api_key: "sk-test-secret-key-xyz".into(),
            app_id: String::new(),
            model: "gpt-4o-mini".into(),
            target: "en".into(),
            proxy: String::new(),
        };
        let out = translate_with(&settings, "zh_CN", "你好").unwrap();
        assert_eq!(out, "Hello from mock");
        let _ = h.join();
        let req = seen.lock().unwrap().clone();
        assert!(
            req.contains("Authorization: Bearer sk-test-secret-key-xyz"),
            "missing bearer in {req:?}"
        );
        assert!(req.contains("gpt-4o-mini"), "missing model in {req:?}");
        assert!(
            !req.contains("translate sk-test"),
            "request dump should not be our log line"
        );
    }

    #[test]
    fn openai_mock_401_does_not_echo_key() {
        let json = r#"{"error":{"message":"Incorrect API key provided: sk-test-secret-key-xyz"}}"#;
        let (url, _, h) = spawn_mock(401, json);
        let settings = TranslateSettings {
            provider: "openai".into(),
            api_url: url,
            api_key: "sk-test-secret-key-xyz".into(),
            app_id: String::new(),
            model: "gpt-4o-mini".into(),
            target: "en".into(),
            proxy: String::new(),
        };
        let err = translate_with_opts(&settings, "en", "hello", false).unwrap_err();
        let _ = h.join();
        assert!(!err.contains("sk-test-secret-key-xyz"), "leaked key: {err}");
        assert!(err.contains("***") || err.contains("Invalid") || err.contains("Incorrect"));
    }

    #[test]
    fn target_follows_settings_then_ui() {
        let mut s = TranslateSettings::default();
        assert_eq!(target_for(&s, "zh_CN"), "zh-Hans");
        assert_eq!(target_for(&s, "en"), "en");
        s.target = "ja".into();
        assert_eq!(target_for(&s, "zh_CN"), "ja");
    }

    #[test]
    fn deepl_mock_translates_form_post() {
        let json = r#"{"translations":[{"detected_source_language":"ZH","text":"Hello DeepL"}]}"#;
        let (url, seen, h) = spawn_mock(200, json);
        let settings = TranslateSettings {
            provider: "deepl".into(),
            api_url: url,
            api_key: "deepl-secret-xyz".into(),
            app_id: String::new(),
            model: String::new(),
            target: "en".into(),
            proxy: String::new(),
        };
        let out = translate_with(&settings, "zh_CN", "你好").unwrap();
        assert_eq!(out, "Hello DeepL");
        let _ = h.join();
        let req = seen.lock().unwrap().clone();
        assert!(
            req.contains("DeepL-Auth-Key deepl-secret-xyz"),
            "missing DeepL auth in {req:?}"
        );
        assert!(req.contains("target_lang=EN"), "missing target in {req:?}");
    }
}
