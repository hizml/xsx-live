use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::{Bytes, Incoming}, header, server::conn::http1, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, net::SocketAddr, path::{Path, PathBuf}, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::{watch, Mutex}};
use crate::auth::XboxAuth;
use crate::control::{self, Action, Command, ControlState};
use crate::login::{self, LoginAttempt, ParsedCallback};

const CONTROL_BODY_LIMIT: usize = 1024;
// 跳转后的网址可能很长，登录请求放宽一些
const LOGIN_BODY_LIMIT: usize = 4096;
// 每个连接的总限时是 10 秒，而换取令牌要联网；最多同步等这么久，超时后由后台继续完成
const LOGIN_WAIT: Duration = Duration::from_secs(8);
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

struct App {
    bind: SocketAddr,
    csrf: String,
    control: watch::Sender<ControlState>,
    state_dir: PathBuf,
    files: HashMap<&'static str, (&'static str, Bytes)>,
    http: reqwest::Client,
    media_base: String,
    rtmp: String,
    login: Arc<Mutex<LoginSlot>>,
}

#[derive(Default)]
struct LoginSlot {
    attempt: Option<LoginAttempt>,
    busy: bool,
    last_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginCallbackRequest { url: String }

enum LoginOutcome { Done, Pending, Failed(String) }

type Reply = Response<Full<Bytes>>;
fn response(status: StatusCode, mime: &str, body: impl Into<Bytes>) -> Reply {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONNECTION, "close")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "same-origin")
        .header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; frame-src 'self' http: https:; object-src 'none'; base-uri 'none'; frame-ancestors 'self'")
        .body(Full::new(body.into())).expect("valid response")
}
fn json_response(status: StatusCode, body: Value) -> Reply {
    response(status, "application/json; charset=utf-8", serde_json::to_vec(&body).expect("JSON response"))
}
fn error(status: StatusCode, message: &str) -> Reply {
    json_response(status, json!({"error":message}))
}
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}
fn text_page(title: &str, message: &str) -> Reply {
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title></head><body><h1>{}</h1><p>{}</p><p><a href=\"/\">返回串流管理页</a></p></body></html>",
        escape_html(title), escape_html(title), escape_html(message)
    );
    response(StatusCode::OK, "text/html; charset=utf-8", body)
}

type Files = HashMap<&'static str, (&'static str, Bytes)>;
// 管理页的静态文件编进程序，镜像自带，不依赖配置目录里有没有这些文件。
// 每一项是：访问路径、源码里的文件名、类型、内容
const EMBEDDED_FILES: [(&str, &str, &str, &[u8]); 3] = [
    ("/", "index.html", "text/html; charset=utf-8", include_bytes!("../../dashboard/index.html")),
    ("/app.js", "app.js", "text/javascript; charset=utf-8", include_bytes!("../../dashboard/app.js")),
    ("/style.css", "style.css", "text/css; charset=utf-8", include_bytes!("../../dashboard/style.css")),
];
fn embedded_files() -> Files {
    EMBEDDED_FILES.iter().map(|(route, _, mime, body)| (*route, (*mime, Bytes::from_static(body)))).collect()
}
// 从目录读，页面改动不用重新编译就能看效果。三个路由对应的文件必须都放进去
fn load_files(dir: &Path) -> Result<Files> {
    EMBEDDED_FILES.iter().map(|(route, name, mime, _)| {
        let body = Bytes::from(std::fs::read(dir.join(name)).context("Dashboard assets missing")?);
        Ok((*route, (*mime, body)))
    }).collect()
}

pub async fn start(control: watch::Sender<ControlState>) -> Result<()> {
    let bind: SocketAddr = std::env::var("DASHBOARD_BIND").unwrap_or_else(|_| "0.0.0.0:8099".into())
        .parse().context("DASHBOARD_BIND 要写成「IP:端口」")?;
    // 管理页的文件默认用编进程序的那一份；设置了 DASHBOARD_DIR 才改从目录读
    let files = match std::env::var("DASHBOARD_DIR") {
        Ok(dir) => load_files(Path::new(&dir))?,
        Err(_) => embedded_files(),
    };
    let mut random = [0u8; 32]; getrandom::fill(&mut random).expect("OS randomness");
    let csrf = random.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let listener = TcpListener::bind(bind).await.context("Dashboard listen")?;
    let app = Arc::new(App {
        bind, csrf, control,
        state_dir: PathBuf::from(std::env::var("STATE_DIR").unwrap_or_else(|_| "/state".into())),
        files, http: reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?,
        media_base: std::env::var("MEDIA_HTTP_BASE").unwrap_or_else(|_| "http://127.0.0.1:8888".into()),
        rtmp: std::env::var("OUTPUT_RTMP").unwrap_or_else(|_| "rtmp://127.0.0.1:1935/live/xsx".into()),
        login: Arc::new(Mutex::new(LoginSlot::default())),
    });
    crate::log!("DASHBOARD listening={bind}");
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => { tokio::time::sleep(Duration::from_secs(1)).await; continue; }
            };
            let state = app.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| handle(req, state.clone()));
                let mut builder = http1::Builder::new();
                builder.keep_alive(false).max_buf_size(16384);
                let _ = tokio::time::timeout(Duration::from_secs(10), builder.serve_connection(io, service)).await;
            });
        }
    });
    Ok(())
}

// 取出请求指向的管理页地址（Host，形如「IP:端口」），不接受的形式返回 None。
// Host 必须是 IP 加端口，端口和监听端口一致；带域名的一律不接受，否则别的网站可以把自己的域名
// 解析到这台机器上冒充管理页（DNS 重绑定）。所以监听地址可以放心写成 0.0.0.0，用机器的任何一个
// IP 都能打开管理页，但用域名访问不了
fn request_host(headers: &hyper::HeaderMap, bind: SocketAddr) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let addr: SocketAddr = host.parse().ok()?;
    (addr.port() == bind.port()).then(|| host.to_owned())
}
// 带 Origin 时它必须和 Host 同源
fn origin_allowed(headers: &hyper::HeaderMap, bind: SocketAddr) -> bool {
    let Some(host) = request_host(headers, bind) else { return false };
    let origin = format!("http://{host}");
    headers.get(header::ORIGIN).is_none_or(|v| v.to_str().ok() == Some(origin.as_str()))
}

fn receiver_state(app: &App) -> Value {
    std::fs::read(app.state_dir.join("status.json")).ok()
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({"state":"unknown"}))
}

// 没有凭据，或者最近一次失败是凭据失效，才允许登录；凭据有效时不给覆盖的机会
fn can_login(configured: bool, receiver: &Value) -> bool {
    let reason = receiver["details"]["reason"].as_str().unwrap_or("");
    !configured || login::is_auth_failure(reason)
}

async fn auth_status(app: &App, receiver: &Value) -> Value {
    let configured = XboxAuth::new().load_cached_tokens().await.unwrap_or(false);
    let slot = app.login.lock().await;
    json!({
        "configured": configured,
        "can_login": can_login(configured, receiver),
        "loopback": login::uses_loopback(),
        "busy": slot.busy,
        "last_error": slot.last_error,
    })
}

async fn status(app: &App, host: &str) -> Value {
    let receiver = receiver_state(app);
    let control = app.control.borrow().clone();
    let media = match app.http.get("http://127.0.0.1:1985/api/v1/streams/").send().await {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => v["streams"].as_array().and_then(|items| items.iter().find(|s|
                s["app"] == "live" && s["name"] == "xsx" && s["publish"]["active"] == true
            )).map(|s| json!({"active":true,"video":s["video"],"audio":s["audio"],"kbps":s["kbps"]["recv_30s"],"clients":s["clients"]})).unwrap_or(json!({"active":false})),
            Err(_) => json!({"active":false}),
        },
        Err(_) => json!({"active":false}),
    };
    let base = app.media_base.trim_end_matches('/');
    json!({
        "now":chrono::Utc::now().timestamp(),
        "version":crate::VERSION,
        "control":control,
        "receiver":receiver,
        "media":media,
        "auth": auth_status(app, &receiver).await,
        "csrf_token":app.csrf,
        "links":{
            "dashboard":format!("http://{host}/"),
            "browser":format!("{base}/xsx/index.html"),
            "flv":format!("{base}/live/xsx.flv"),
            "rtmp":app.rtmp,
            "preview":format!("{base}/xsx/index.html?audio=0")
        }
    })
}

async fn handle(req: Request<Incoming>, app: Arc<App>) -> Result<Reply, Infallible> {
    let path = req.uri().path().to_owned();
    // 微软登录完成后由浏览器跨站跳到 /auth/callback，请求里不带本页的 Host 和 Origin，不能走常规的来源校验；
    // 它的安全性由 state 和 PKCE 保证
    let is_login_callback = req.method() == Method::GET && path.ends_with("/auth/callback");
    if !is_login_callback && !origin_allowed(req.headers(), app.bind) {
        return Ok(error(StatusCode::FORBIDDEN, "不允许的访问来源"));
    }
    if req.method() == Method::GET {
        if is_login_callback { return Ok(login_callback_page(req.uri().query(), &app).await); }
        if path == "/api/status" {
            let host = request_host(req.headers(), app.bind).unwrap_or_default();
            return Ok(json_response(StatusCode::OK, status(&app, &host).await));
        }
        if path == "/favicon.ico" { return Ok(response(StatusCode::NO_CONTENT, "image/x-icon", Bytes::new())); }
        if let Some((mime, data)) = app.files.get(path.as_str()) {
            return Ok(response(StatusCode::OK, mime, data.clone()));
        }
        return Ok(error(StatusCode::NOT_FOUND, "页面不存在"));
    }
    if req.method() != Method::POST {
        return Ok(error(StatusCode::METHOD_NOT_ALLOWED, "不支持的操作"));
    }
    Ok(match path.as_str() {
        "/api/control" => control_command(req, &app).await,
        "/api/auth/start" | "/api/auth/callback" => login_command(req, &app, &path).await,
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "不支持的操作"),
    })
}

async fn read_json_body(req: Request<Incoming>, app: &App, limit: usize) -> Result<Bytes, Reply> {
    if req.headers().get("X-XSX-Token").and_then(|v|v.to_str().ok()) != Some(app.csrf.as_str()) {
        return Err(error(StatusCode::FORBIDDEN, "页面已更新，请刷新后再操作"));
    }
    if !req.headers().get(header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).is_some_and(|v|v.split(';').next()==Some("application/json")) {
        return Err(error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "需要 JSON 请求"));
    }
    match Limited::new(req.into_body(), limit).collect().await {
        Ok(v) => Ok(v.to_bytes()),
        Err(_) => Err(error(StatusCode::PAYLOAD_TOO_LARGE, "请求过大")),
    }
}

async fn control_command(req: Request<Incoming>, app: &Arc<App>) -> Reply {
    let body = match read_json_body(req, app, CONTROL_BODY_LIMIT).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let parsed: Result<Command, _> = serde_json::from_slice(&body);
    let command = match parsed {
        Ok(command) if command.is_well_formed() => command,
        _ => return error(StatusCode::BAD_REQUEST, "不支持的控制指令"),
    };
    let changed = match control::apply_action(&app.control, &command).await {
        Ok(changed) => changed,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "保存开关状态失败"),
    };
    let control = app.control.borrow().clone();
    json_response(if changed {StatusCode::ACCEPTED} else {StatusCode::OK},
        json!({"accepted":true,"changed":changed,"control":control}))
}

async fn login_command(req: Request<Incoming>, app: &Arc<App>, path: &str) -> Reply {
    let body = match read_json_body(req, app, LOGIN_BODY_LIMIT).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let mut slot = app.login.lock().await;
    if slot.busy { return error(StatusCode::CONFLICT, "上一个登录仍在处理中，请稍候"); }
    if path == "/api/auth/start" { return start_login(app, &mut slot).await; }
    let request: LoginCallbackRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "无法识别的登录请求"),
    };
    let started = match login::parse_callback(&request.url) {
        Some(ParsedCallback::Code { code, state }) => begin_login(app, &mut slot, code, state),
        Some(ParsedCallback::Failed(reason)) => Err(format!("微软返回错误：{reason}")),
        None => Err("网址里没有登录凭证，请复制跳转后地址栏里的完整网址".to_owned()),
    };
    drop(slot);
    match started {
        Ok(()) => login_json_reply(wait_for_login(app).await),
        Err(message) => error(StatusCode::BAD_REQUEST, &message),
    }
}

async fn start_login(app: &Arc<App>, slot: &mut LoginSlot) -> Reply {
    let configured = XboxAuth::new().load_cached_tokens().await.unwrap_or(false);
    if !can_login(configured, &receiver_state(app)) {
        return error(StatusCode::FORBIDDEN, "凭据有效，无需重新登录");
    }
    let attempt = LoginAttempt::new();
    let reply = json!({
        "url": attempt.authorize_url(),
        "loopback": login::uses_loopback(),
        "expires_in": login::ATTEMPT_TTL.as_secs(),
    });
    slot.attempt = Some(attempt);
    slot.last_error = None;
    json_response(StatusCode::OK, reply)
}

// 校验 state 后在后台换取令牌并写入凭据。调用方持有 slot，并保证此刻没有进行中的登录
fn begin_login(app: &Arc<App>, slot: &mut LoginSlot, code: String, state: String) -> Result<(), String> {
    let attempt = match slot.attempt.take() {
        Some(attempt) if !attempt.is_expired() => attempt,
        _ => return Err("登录已过期或尚未开始，请重新开始登录".to_owned()),
    };
    if !attempt.matches(&state) {
        return Err("登录校验失败，请回到管理页重新开始登录".to_owned());
    }
    slot.busy = true;
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let outcome = attempt.complete(&code).await;
        let mut slot = app.login.lock().await;
        slot.busy = false;
        slot.last_error = outcome.as_ref().err().map(|e| e.to_string());
        drop(slot);
        match outcome {
            Ok(()) => {
                crate::log!("LOGIN ok=true");
                // 串流开着的话立即重连，让新的凭据马上生效
                let enabled = app.control.borrow().enabled;
                if enabled { let _ = control::apply_action(&app.control, &Command { action: Action::Restart, enabled: None }).await; }
            }
            Err(e) => crate::log!("LOGIN ok=false reason={e}"),
        }
    });
    Ok(())
}

async fn wait_for_login(app: &App) -> LoginOutcome {
    tokio::select! {
        _ = tokio::time::sleep(LOGIN_WAIT) => {},
        _ = login_finished(app) => {},
    }
    let slot = app.login.lock().await;
    if slot.busy { return LoginOutcome::Pending; }
    match &slot.last_error {
        Some(reason) => LoginOutcome::Failed(reason.clone()),
        None => LoginOutcome::Done,
    }
}

async fn login_finished(app: &App) {
    while app.login.lock().await.busy {
        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
    }
}

fn login_json_reply(outcome: LoginOutcome) -> Reply {
    match outcome {
        LoginOutcome::Done => json_response(StatusCode::OK, json!({"ok":true})),
        LoginOutcome::Pending => json_response(StatusCode::ACCEPTED, json!({"pending":true})),
        LoginOutcome::Failed(reason) => error(StatusCode::BAD_GATEWAY, &format!("登录失败：{reason}")),
    }
}

async fn login_callback_page(query: Option<&str>, app: &Arc<App>) -> Reply {
    let started = match login::parse_callback(query.unwrap_or("")) {
        Some(ParsedCallback::Code { code, state }) => {
            let mut slot = app.login.lock().await;
            if slot.busy { Err("另一个登录正在处理中，请稍候".to_owned()) } else { begin_login(app, &mut slot, code, state) }
        }
        Some(ParsedCallback::Failed(reason)) => Err(format!("微软返回错误：{reason}")),
        None => Err("网址里没有登录凭证".to_owned()),
    };
    let (title, message) = match started {
        Ok(()) => match wait_for_login(app).await {
            LoginOutcome::Done => ("Xbox 登录成功", "可以关闭此页面，回到串流管理页。".to_owned()),
            LoginOutcome::Pending => ("正在完成登录", "仍在换取凭据，请稍后回到串流管理页查看。".to_owned()),
            LoginOutcome::Failed(reason) => ("Xbox 登录未完成", format!("{reason}。请回到串流管理页重新开始。")),
        },
        Err(message) => ("Xbox 登录未完成", format!("{message}。请回到串流管理页重新开始。")),
    };
    text_page(title, &message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> hyper::HeaderMap {
        let mut map = hyper::HeaderMap::new();
        for (name, value) in pairs { map.insert(hyper::header::HeaderName::try_from(*name).unwrap(), value.parse().unwrap()); }
        map
    }

    // 测试里一律用 RFC 5737 留给文档的示例地址（192.0.2.0/24）和 example 域名，不要写真实的局域网地址或主机名
    fn bind() -> SocketAddr { "0.0.0.0:8099".parse().unwrap() }

    #[test]
    fn host_must_be_an_ip_with_the_listening_port() {
        let host = |value: &str| request_host(&headers(&[("host", value)]), bind());
        assert_eq!(host("192.0.2.10:8099").as_deref(), Some("192.0.2.10:8099"));
        assert_eq!(host("127.0.0.1:8099").as_deref(), Some("127.0.0.1:8099"), "本机访问也行");
        assert!(host("[::1]:8099").is_some(), "IPv6 字面量也行");
        assert!(host("media.local:8099").is_none(), "域名一律不接受，这是防 DNS 重绑定的关键");
        assert!(host("nas.example.com:8099").is_none());
        assert!(host("192.0.2.10:1935").is_none(), "端口要和监听端口一致");
        assert!(host("192.0.2.10").is_none(), "没写端口不行");
    }

    #[test]
    fn origin_must_match_host() {
        let same = headers(&[("host", "127.0.0.1:8099"), ("origin", "http://127.0.0.1:8099")]);
        let cross = headers(&[("host", "127.0.0.1:8099"), ("origin", "http://evil.example")]);
        assert!(origin_allowed(&same, bind()));
        assert!(!origin_allowed(&cross, bind()), "别的网站发起的请求要拒绝");
        assert!(origin_allowed(&headers(&[("host", "127.0.0.1:8099")]), bind()), "不带 Origin 的普通访问");
        assert!(!origin_allowed(&headers(&[("host", "evil.example:8099"), ("origin", "http://evil.example:8099")]), bind()));
    }

    #[test]
    fn embedded_dashboard_is_complete() {
        let files = embedded_files();
        for route in ["/", "/app.js", "/style.css"] {
            assert!(files.get(route).is_some_and(|(_, body)| !body.is_empty()), "缺少 {route}");
        }
        let page = String::from_utf8_lossy(&files["/"].1);
        assert!(page.contains("src=\"/app.js\"") && page.contains("href=\"/style.css\""), "页面引用的路径要和路由对得上");
    }

    #[test]
    fn directory_override_replaces_embedded_files() {
        let dir = std::env::temp_dir().join(format!("xsx-dashboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["index.html", "app.js", "style.css"] { std::fs::write(dir.join(name), name).unwrap(); }
        let files = load_files(&dir).unwrap();
        assert_eq!(files["/"].1, "index.html");
        assert_eq!(files["/style.css"].0, "text/css; charset=utf-8");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_override_fails_loudly_when_a_file_is_missing() {
        let dir = std::env::temp_dir().join(format!("xsx-dashboard-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_files(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
