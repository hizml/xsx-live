// 管理页里的 Xbox 账号登录：OAuth 授权码加 PKCE，做法与上游 kite 一致。
// 令牌只写入凭据文件，不进入日志和接口响应。
use crate::auth::XboxAuth;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::OnceLock, time::{Duration, Instant}};

const AUTHORIZE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const SCOPE: &str = "XboxLive.signin XboxLive.offline_access";
// 上游 kite 内置的 Azure 应用（公开客户端，无密钥），可用 XBOX_CLIENT_ID 换成自己注册的应用
const DEFAULT_CLIENT_ID: &str = "cd779487-1790-4b69-8304-d845a52c9b79";
// 微软只允许 http://localhost 走明文 HTTP，其他跳回地址必须是 HTTPS，并且要在自己注册的应用里登记
const LOOPBACK_REDIRECT_URI: &str = "http://localhost";
pub const ATTEMPT_TTL: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const VERIFIER_BYTES: usize = 32;
const STATE_BYTES: usize = 16;
// 续期失败时原因里会出现的标记。只有出现这些才允许重新登录，避免局域网里的其他设备覆盖掉有效的凭据
const AUTH_FAILURE_MARKERS: [&str; 5] = ["Authorization unavailable", "AUTH_REQUIRED", "invalid_grant", "HTTP 401", "invalid_client"];

/// 用户粘贴进来的跳转结果：授权码，或者微软直接返回的错误
#[derive(Debug)]
pub enum ParsedCallback {
    Code { code: String, state: String },
    Failed(String),
}

fn env_setting(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
}

fn redirect_uri() -> &'static str {
    static REDIRECT_URI: OnceLock<String> = OnceLock::new();
    REDIRECT_URI.get_or_init(|| match env_setting("AUTH_REDIRECT_URI") {
        Some(value) if value.starts_with("https://") => value,
        Some(value) => {
            eprintln!("AUTH_REDIRECT_URI 必须以 https:// 开头，已改用 {LOOPBACK_REDIRECT_URI}：{value}");
            LOOPBACK_REDIRECT_URI.to_owned()
        }
        None => LOOPBACK_REDIRECT_URI.to_owned(),
    })
}

/// 没有配置 HTTPS 跳回地址时，微软会把浏览器带到本机的 localhost，需要用户把地址栏里的网址粘贴回管理页
pub fn uses_loopback() -> bool {
    redirect_uri() == LOOPBACK_REDIRECT_URI
}

pub fn is_auth_failure(reason: &str) -> bool {
    AUTH_FAILURE_MARKERS.iter().any(|marker| reason.contains(marker))
}

fn random_token(length: usize) -> String {
    let mut bytes = vec![0u8; length];
    getrandom::fill(&mut bytes).expect("OS randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn encode_component(value: &str) -> String {
    value.bytes().map(|byte| match byte {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (byte as char).to_string(),
        _ => format!("%{byte:02X}"),
    }).collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let (mut decoded, mut index) = (Vec::with_capacity(bytes.len()), 0);
    while index < bytes.len() {
        let escaped = bytes.get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match escaped {
            Some(byte) => { decoded.push(byte); index += 3; }
            None => { decoded.push(bytes[index]); index += 1; }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// 从跳转后的网址里取出授权码和 state；也接受只粘贴了查询串的情况
pub fn parse_callback(raw: &str) -> Option<ParsedCallback> {
    let trimmed = raw.trim();
    let query = match trimmed.split_once('?') {
        Some((_, query)) => query,
        None if trimmed.contains('=') => trimmed,
        None => return None,
    };
    let fields: HashMap<String, String> = query.split('#').next().unwrap_or("").split('&')
        .filter_map(|pair| pair.split_once('=').map(|(key, value)| (key.to_owned(), percent_decode(value))))
        .collect();
    if let Some(reason) = fields.get("error") {
        return Some(ParsedCallback::Failed(reason.clone()));
    }
    let (code, state) = (fields.get("code")?, fields.get("state")?);
    if code.is_empty() || state.is_empty() {
        return None;
    }
    Some(ParsedCallback::Code { code: code.clone(), state: state.clone() })
}

/// 一次进行中的登录：PKCE 校验串和 state 只放在内存里，十分钟内有效
pub struct LoginAttempt {
    verifier: String,
    state: String,
    client_id: String,
    created_at: Instant,
}

impl LoginAttempt {
    pub fn new() -> Self {
        Self {
            verifier: random_token(VERIFIER_BYTES),
            state: random_token(STATE_BYTES),
            client_id: env_setting("XBOX_CLIENT_ID").unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned()),
            created_at: Instant::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > ATTEMPT_TTL
    }

    pub fn authorize_url(&self) -> String {
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(self.verifier.as_bytes()));
        let query = [
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri()),
            ("response_mode", "query"),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", self.state.as_str()),
            ("prompt", "select_account"),
        ].iter().map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
            .collect::<Vec<_>>().join("&");
        format!("{AUTHORIZE_URL}?{query}")
    }

    /// 不随比较提前返回，避免通过响应时间猜测 state
    pub fn matches(&self, state: &str) -> bool {
        self.state.len() == state.len()
            && self.state.bytes().zip(state.bytes()).fold(0u8, |difference, (left, right)| difference | (left ^ right)) == 0
    }

    /// 用授权码换取长期令牌并写入凭据文件
    pub async fn complete(&self, code: &str) -> std::result::Result<(), LoginFailure> {
        let refresh_token = self.exchange(code).await.map_err(LoginFailure::Exchange)?;
        XboxAuth::new().store_session(&self.client_id, &refresh_token, SCOPE).await.map_err(|e| LoginFailure::Store(e.to_string()))
    }

    async fn exchange(&self, code: &str) -> std::result::Result<String, ExchangeFailure> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).redirect(reqwest::redirect::Policy::none()).build()
            .map_err(unreachable_exchange)?;
        let form = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri()),
            ("code_verifier", self.verifier.as_str()),
            ("scope", SCOPE),
        ];
        let response = client.post(TOKEN_URL).form(&form).send().await.map_err(unreachable_exchange)?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        take_refresh_token(status, &body)
    }
}

/// 页面用哪种语言：按浏览器的 Accept-Language 选
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang { Zh, En }

impl Lang {
    // 没带 Accept-Language 时按中文；带了就看排第一的语言是不是中文，不是的一律按英文
    pub fn from_accept_language(value: Option<&str>) -> Self {
        let Some(value) = value else { return Self::Zh };
        let first = value.split(',').next().unwrap_or("").trim().to_ascii_lowercase();
        if first.starts_with("zh") { Self::Zh } else { Self::En }
    }
}

/// 向微软换取凭据失败的原因。code 是归好类的标识（管理页按它给出当前语言的说明），
/// detail 是微软返回的状态码和错误码，只用于排查
#[derive(Debug, Clone)]
pub struct ExchangeFailure { pub code: &'static str, pub detail: String }

// 微软的回应里有长期令牌就算成功；没有的话把原因归类后报告失败
fn take_refresh_token(status: u16, body: &Value) -> std::result::Result<String, ExchangeFailure> {
    let token = body.get("refresh_token").and_then(Value::as_str).filter(|token| !token.is_empty());
    if let Some(token) = token { return Ok(token.to_owned()); }
    let code = body.get("error").and_then(Value::as_str).unwrap_or("missing_refresh_token");
    // 微软的错误描述里写着具体原因（例如跳回地址不匹配的 AADSTS50011），只用来归类，不往外发
    let description = body.get("error_description").and_then(Value::as_str).unwrap_or("");
    Err(describe_exchange_failure(status, code, description))
}

// 没拿到微软的回应（网络不通、回应不是 JSON 等）
fn unreachable_exchange(error: reqwest::Error) -> ExchangeFailure {
    ExchangeFailure { code: "exchange_error", detail: error.to_string().chars().take(200).collect() }
}

/// 登录失败：换取凭据那一步的失败带归好类的标识，写入凭据文件失败的只有原始错误
#[derive(Debug, Clone)]
pub enum LoginFailure { Exchange(ExchangeFailure), Store(String) }

impl std::fmt::Display for LoginFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exchange(failure) => write!(f, "{}: {}", failure.code, failure.detail),
            Self::Store(error) => write!(f, "store_failed: {error}"),
        }
    }
}

// 把微软的错误码归到上面几类里，原始的状态码和错误码留在 detail 里便于排查
fn describe_exchange_failure(status: u16, code: &str, description: &str) -> ExchangeFailure {
    const KINDS: [(&str, &str); 3] = [("invalid_grant", "invalid_grant"), ("invalid_client", "invalid_client"), ("missing_refresh_token", "no_refresh_token")];
    let kind = KINDS.iter().find(|(name, _)| *name == code).map(|(_, kind)| *kind)
        .unwrap_or(if description.contains("AADSTS50011") { "redirect_mismatch" } else { "rejected" });
    ExchangeFailure { code: kind, detail: format!("{status} code={code}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(raw: &str) -> (String, String) {
        match parse_callback(raw) {
            Some(ParsedCallback::Code { code, state }) => (code, state),
            other => panic!("expected code, got {other:?}"),
        }
    }

    #[test]
    fn parses_loopback_redirect() {
        let (code, state) = code("http://localhost/?code=M.C521_BAY.2.U.abc&state=xyz&session_state=zzz");
        assert_eq!((code.as_str(), state.as_str()), ("M.C521_BAY.2.U.abc", "xyz"));
    }

    #[test]
    fn parses_bare_query_and_decodes_percent() {
        let (code, state) = code("  code=a%2Fb%3Dc&state=s%20t  ");
        assert_eq!((code.as_str(), state.as_str()), ("a/b=c", "s t"));
    }

    #[test]
    fn reports_provider_error() {
        assert!(matches!(parse_callback("http://localhost/?error=access_denied&error_description=x"),
            Some(ParsedCallback::Failed(reason)) if reason == "access_denied"));
    }

    #[test]
    fn rejects_incomplete_input() {
        assert!(parse_callback("http://localhost/").is_none());
        assert!(parse_callback("code=only").is_none());
        assert!(parse_callback("just-a-code").is_none());
        assert!(parse_callback("").is_none());
    }

    #[test]
    fn authorize_url_carries_pkce_and_state() {
        let url = LoginAttempt::new().authorize_url();
        for fragment in ["code_challenge_method=S256", "response_type=code", "prompt=select_account", "code_challenge="] {
            assert!(url.contains(fragment), "missing {fragment} in {url}");
        }
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains(&format!("redirect_uri={}", encode_component(redirect_uri()))));
    }

    #[test]
    fn state_must_match_exactly() {
        let attempt = LoginAttempt::new();
        let state = attempt.state.clone();
        assert!(attempt.matches(&state));
        assert!(!attempt.matches(&format!("{state}x")));
        assert!(!attempt.matches(&state[..state.len() - 1]));
    }

    #[test]
    fn only_refresh_failures_count_as_auth_failures() {
        assert!(is_auth_failure("Authorization unavailable"));
        assert!(is_auth_failure("xHome login: HTTP 401 - expired"));
        assert!(is_auth_failure("HTTP 400 code=invalid_grant"));
        assert!(!is_auth_failure("CONSOLE_OFFLINE"));
        assert!(!is_auth_failure(""));
    }

    #[test]
    fn component_encoding_is_rfc3986() {
        assert_eq!(encode_component("XboxLive.signin offline~-_."), "XboxLive.signin%20offline~-_.");
        assert_eq!(encode_component("https://a/b?c=d"), "https%3A%2F%2Fa%2Fb%3Fc%3Dd");
    }

    #[test]
    fn exchange_failures_are_classified() {
        let expired = describe_exchange_failure(400, "invalid_grant", "AADSTS70000: The provided value for the 'code' parameter is not valid.");
        assert_eq!((expired.code, expired.detail.as_str()), ("invalid_grant", "400 code=invalid_grant"));
        let mismatch = describe_exchange_failure(200, "invalid_request", "AADSTS50011: The redirect URI specified in the request does not match");
        assert_eq!(mismatch.code, "redirect_mismatch");
        assert_eq!(describe_exchange_failure(400, "unauthorized_client", "").code, "rejected");
        assert_eq!(describe_exchange_failure(200, "missing_refresh_token", "").code, "no_refresh_token");
        assert_eq!(describe_exchange_failure(500, "server_error", "something broke").code, "rejected");
        // 各类失败的标识都有对应的说明文字，由 dashboard 的测试逐个核对
        for kind in ["invalid_grant", "redirect_mismatch", "rejected", "no_refresh_token"] {
            let failure = LoginFailure::Exchange(ExchangeFailure { code: kind, detail: "400 code=x".to_owned() });
            assert_eq!(failure.to_string(), format!("{kind}: 400 code=x"));
        }
    }

    #[test]
    fn page_language_follows_the_browser() {
        assert_eq!(Lang::from_accept_language(None), Lang::Zh, "没带语言时按中文");
        assert_eq!(Lang::from_accept_language(Some("zh-CN,zh;q=0.9,en;q=0.8")), Lang::Zh);
        assert_eq!(Lang::from_accept_language(Some("en-US,en;q=0.9")), Lang::En);
        assert_eq!(Lang::from_accept_language(Some("ja-JP,ja;q=0.9")), Lang::En, "其他语言退回英文");
    }
}
