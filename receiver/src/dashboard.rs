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
use crate::login::{self, LoginAttempt, LoginFailure, ParsedCallback};

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
    last_failure: Option<LoginFailure>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginCallbackRequest { url: String }

enum LoginOutcome { Done, Pending, Failed(LoginFailure) }

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
// 服务端给出的文字：接口的报错，以及登录跳回页面上的话。每一项是：标识、中文、英文。
// 接口报错时带上标识（code），管理页按它翻译成当前语言；跳回页面的语言按浏览器的 Accept-Language 选
const TEXTS: [(&str, &str, &str); 29] = [
    ("forbidden_origin", "不允许的访问来源", "This address is not allowed to open the dashboard"),
    ("csrf_missing", "页面已更新，请刷新后再操作", "The page has been updated. Refresh and try again"),
    ("json_required", "需要 JSON 请求", "The request must be JSON"),
    ("body_too_large", "请求过大", "The request is too large"),
    ("not_found", "页面不存在", "Page not found"),
    ("method_not_allowed", "不支持的操作", "Unsupported request"),
    ("unknown_control", "不支持的控制指令", "Unknown control command"),
    ("save_failed", "保存开关状态失败", "Could not save the switch state"),
    ("bad_login_request", "无法识别的登录请求", "Unrecognized sign-in request"),
    ("login_busy", "另一个登录正在处理中，请稍候", "Another sign-in is still in progress, please wait"),
    ("login_not_needed", "凭据有效，无需重新登录", "The saved sign-in is still valid. No need to sign in again"),
    ("credentials_missing", "网址里没有登录凭证，请复制跳转后地址栏里的完整网址", "The URL contains no sign-in credentials. Copy the complete URL from the address bar after the redirect"),
    ("expired", "登录已过期或尚未开始，请重新开始登录", "The sign-in has expired or was never started. Please start again"),
    ("state_mismatch", "登录校验失败，请回到管理页重新开始登录", "Sign-in verification failed. Go back to the dashboard and start again"),
    ("login_provider_error", "微软返回了错误", "Microsoft returned an error"),
    ("invalid_grant", "授权码无效或已过期", "The authorization code is invalid or has expired"),
    ("invalid_client", "应用 ID 无效，请检查 XBOX_CLIENT_ID", "The app ID is invalid. Check XBOX_CLIENT_ID"),
    ("no_refresh_token", "微软没有返回长期令牌", "Microsoft did not return a refresh token"),
    ("redirect_mismatch", "跳回地址与应用里登记的不一致，请检查 AUTH_REDIRECT_URI", "The redirect address does not match the one registered for the app. Check AUTH_REDIRECT_URI"),
    ("rejected", "微软拒绝了这次登录", "Microsoft rejected this sign-in"),
    ("exchange_error", "向微软换取凭据失败", "Could not reach Microsoft to exchange the credentials"),
    ("store_failed", "凭据写入失败", "Could not save the credentials"),
    ("success_title", "Xbox 登录成功", "Xbox sign-in successful"),
    ("success_body", "可以关闭此页面，回到串流管理页。", "You can close this page and go back to the dashboard."),
    ("pending_title", "正在完成登录", "Finishing sign-in"),
    ("pending_body", "仍在换取凭据，请稍后回到串流管理页查看。", "Still exchanging credentials. Check the dashboard in a moment."),
    ("failed_title", "Xbox 登录未完成", "Xbox sign-in not completed"),
    ("failed_body", "请回到串流管理页重新开始。", "Go back to the dashboard and start again."),
    ("back", "返回串流管理页", "Back to the dashboard"),
];
fn text(lang: login::Lang, key: &str) -> &'static str {
    TEXTS.iter().find(|(name, _, _)| *name == key)
        .map(|(_, zh, en)| match lang { login::Lang::Zh => *zh, login::Lang::En => *en })
        .unwrap_or("")
}
// 接口出错时带上固定的代码（code），管理页按代码翻译成当前语言；extra 是补充信息，例如 detail
fn error(status: StatusCode, code: &str) -> Reply {
    error_with(status, code, text(login::Lang::Zh, code), json!({}))
}
fn error_with(status: StatusCode, code: &str, message: &str, extra: Value) -> Reply {
    let mut body = json!({"error":message,"code":code});
    if let Some(extra) = extra.as_object() { body.as_object_mut().expect("JSON 对象").extend(extra.clone()); }
    json_response(status, body)
}
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}
// 登录跳回后显示的页面。每一行正文是一个段落
fn text_page(lang: login::Lang, title: &str, lines: &[String]) -> Reply {
    let tag = match lang { login::Lang::Zh => "zh-CN", login::Lang::En => "en" };
    let paragraphs = lines.iter().map(|line| format!("<p>{}</p>", escape_html(line))).collect::<String>();
    let body = format!(
        "<!doctype html><html lang=\"{tag}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title></head><body><h1>{}</h1>{paragraphs}<p><a href=\"/\">{}</a></p></body></html>",
        escape_html(title), escape_html(title), escape_html(text(lang, "back"))
    );
    response(StatusCode::OK, "text/html; charset=utf-8", body)
}

type Files = HashMap<&'static str, (&'static str, Bytes)>;
// 管理页的静态文件编进程序，镜像自带，不依赖配置目录里有没有这些文件。
// 每一项是：访问路径、源码里的文件名、类型、内容
const EMBEDDED_FILES: [(&str, &str, &str, &[u8]); 4] = [
    ("/", "index.html", "text/html; charset=utf-8", include_bytes!("../../dashboard/index.html")),
    ("/i18n.js", "i18n.js", "text/javascript; charset=utf-8", include_bytes!("../../dashboard/i18n.js")),
    ("/app.js", "app.js", "text/javascript; charset=utf-8", include_bytes!("../../dashboard/app.js")),
    ("/style.css", "style.css", "text/css; charset=utf-8", include_bytes!("../../dashboard/style.css")),
];
fn embedded_files() -> Files {
    EMBEDDED_FILES.iter().map(|(route, _, mime, body)| (*route, (*mime, Bytes::from_static(body)))).collect()
}
// 从目录读，页面改动不用重新编译就能看效果。四个路由对应的文件必须都放进去
fn load_files(dir: &Path) -> Result<Files> {
    EMBEDDED_FILES.iter().map(|(route, name, mime, _)| {
        let body = Bytes::from(std::fs::read(dir.join(name)).context("Dashboard assets missing")?);
        Ok((*route, (*mime, body)))
    }).collect()
}

// 读环境变量，组装管理页各个请求共用的状态
fn build_app(control: watch::Sender<ControlState>, bind: SocketAddr) -> Result<App> {
    // 管理页的文件默认用编进程序的那一份；设置了 DASHBOARD_DIR 才改从目录读
    let files = match std::env::var("DASHBOARD_DIR") {
        Ok(dir) => load_files(Path::new(&dir))?,
        Err(_) => embedded_files(),
    };
    let mut random = [0u8; 32]; getrandom::fill(&mut random).expect("OS randomness");
    let csrf = random.iter().map(|x| format!("{x:02x}")).collect::<String>();
    Ok(App {
        bind, csrf, control,
        state_dir: PathBuf::from(std::env::var("STATE_DIR").unwrap_or_else(|_| "/state".into())),
        files, http: reqwest::Client::builder().timeout(Duration::from_secs(2)).build()?,
        media_base: std::env::var("MEDIA_HTTP_BASE").unwrap_or_else(|_| "http://127.0.0.1:8888".into()),
        rtmp: std::env::var("OUTPUT_RTMP").unwrap_or_else(|_| "rtmp://127.0.0.1:1935/live/xsx".into()),
        login: Arc::new(Mutex::new(LoginSlot::default())),
    })
}

// 一个连接一个任务，整个连接最多 10 秒（登录最多同步等 8 秒）
async fn serve_connection(stream: tokio::net::TcpStream, app: Arc<App>) {
    let service = service_fn(move |req| handle(req, app.clone()));
    let mut builder = http1::Builder::new();
    builder.keep_alive(false).max_buf_size(16384);
    let _ = tokio::time::timeout(Duration::from_secs(10), builder.serve_connection(TokioIo::new(stream), service)).await;
}

pub async fn start(control: watch::Sender<ControlState>) -> Result<()> {
    let bind: SocketAddr = std::env::var("DASHBOARD_BIND").unwrap_or_else(|_| "0.0.0.0:8099".into())
        .parse().context("DASHBOARD_BIND 要写成「IP:端口」")?;
    let app = Arc::new(build_app(control, bind)?);
    let listener = TcpListener::bind(bind).await.context("Dashboard listen")?;
    crate::log!("DASHBOARD listening={bind}");
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => { tokio::time::sleep(Duration::from_secs(1)).await; continue; }
            };
            tokio::spawn(serve_connection(stream, app.clone()));
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
        "last_error": slot.last_failure.as_ref().map(|f| f.to_string()),
        "last_error_code": slot.last_failure.as_ref().map(failure_code),
        "last_error_detail": slot.last_failure.as_ref().map(failure_detail),
    })
}

const MEDIA_API: &str = "http://127.0.0.1:1985/api/v1/streams/";
// 问分发服务的接口：我们推的那一路（live/xsx）现在是不是在出流。接口不通、回应看不懂都按没出流算
async fn media_status(app: &App) -> Value {
    let streams: Option<Value> = match app.http.get(MEDIA_API).send().await {
        Ok(reply) => reply.json::<Value>().await.ok(),
        Err(_) => None,
    };
    let stream = streams.as_ref().and_then(|v| v["streams"].as_array()).and_then(|items|
        items.iter().find(|s| s["app"] == "live" && s["name"] == "xsx" && s["publish"]["active"] == true));
    match stream {
        Some(s) => json!({"active":true,"video":s["video"],"audio":s["audio"],"kbps":s["kbps"]["recv_30s"],"clients":s["clients"]}),
        None => json!({"active":false}),
    }
}

// 分发服务对外的根地址。MEDIA_HTTP_BASE 指向本机回环地址（也就是默认值，说明没有专门配置）时，
// 改用浏览器访问管理页用的 IP：分发服务和管理页跑在同一台机器上，这样别的设备拿到的地址才是对的
fn media_base(configured: &str, host: &str) -> String {
    let configured = configured.trim_end_matches('/');
    let Some((scheme, address)) = configured.split_once("://") else { return configured.to_owned() };
    let (authority, path) = address.split_at(address.find('/').unwrap_or(address.len()));
    let (configured_host, port) = split_authority(authority);
    let Some(access) = host.rsplit_once(':').map(|(ip, _)| ip) else { return configured.to_owned() };
    if !is_loopback(configured_host) { return configured.to_owned(); }
    format!("{scheme}://{access}{}{path}", port.map(|port| format!(":{port}")).unwrap_or_default())
}

// 把「主机[:端口]」拆成主机和端口。IPv6 地址自带冒号，要带着方括号才认得出端口
fn split_authority(authority: &str) -> (&str, Option<&str>) {
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (authority, None),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

// 管理页上给出的各个地址。管理页自己的地址跟着这次访问用的 IP 走
fn links(app: &App, host: &str) -> Value {
    let base = media_base(&app.media_base, host);
    json!({
        "dashboard":format!("http://{host}/"),
        "browser":format!("{base}/xsx/index.html"),
        "flv":format!("{base}/live/xsx.flv"),
        "rtmp":app.rtmp,
        "preview":format!("{base}/xsx/index.html?audio=0")
    })
}

async fn status(app: &App, host: &str) -> Value {
    let receiver = receiver_state(app);
    let control = app.control.borrow().clone();
    json!({
        "now":chrono::Utc::now().timestamp(),
        "version":crate::VERSION,
        "control":control,
        "receiver":receiver,
        "media": media_status(app).await,
        "auth": auth_status(app, &receiver).await,
        "csrf_token":app.csrf,
        "links": links(app, host),
    })
}

// GET 请求：登录跳回页、状态接口、管理页的静态文件
async fn handle_get(req: Request<Incoming>, app: Arc<App>, is_login_callback: bool) -> Reply {
    let path = req.uri().path();
    if is_login_callback {
        let language = req.headers().get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok());
        return login_callback_page(req.uri().query(), &app, login::Lang::from_accept_language(language)).await;
    }
    if path == "/api/status" {
        let host = request_host(req.headers(), app.bind).unwrap_or_default();
        return json_response(StatusCode::OK, status(&app, &host).await);
    }
    if path == "/favicon.ico" { return response(StatusCode::NO_CONTENT, "image/x-icon", Bytes::new()); }
    match app.files.get(path) {
        Some((mime, data)) => response(StatusCode::OK, mime, data.clone()),
        None => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn handle(req: Request<Incoming>, app: Arc<App>) -> Result<Reply, Infallible> {
    let path = req.uri().path().to_owned();
    // 微软登录完成后由浏览器跨站跳到 /auth/callback，请求里不带本页的 Host 和 Origin，不能走常规的来源校验；
    // 它的安全性由 state 和 PKCE 保证
    let is_login_callback = req.method() == Method::GET && path.ends_with("/auth/callback");
    if !is_login_callback && !origin_allowed(req.headers(), app.bind) {
        return Ok(error(StatusCode::FORBIDDEN, "forbidden_origin"));
    }
    if req.method() == Method::GET { return Ok(handle_get(req, app, is_login_callback).await); }
    if req.method() != Method::POST { return Ok(error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")); }
    Ok(match path.as_str() {
        "/api/control" => control_command(req, &app).await,
        "/api/auth/start" | "/api/auth/callback" => login_command(req, &app, &path).await,
        _ => error(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
    })
}

async fn read_json_body(req: Request<Incoming>, app: &App, limit: usize) -> Result<Bytes, Reply> {
    if req.headers().get("X-XSX-Token").and_then(|v|v.to_str().ok()) != Some(app.csrf.as_str()) {
        return Err(error(StatusCode::FORBIDDEN, "csrf_missing"));
    }
    if !req.headers().get(header::CONTENT_TYPE).and_then(|v|v.to_str().ok()).is_some_and(|v|v.split(';').next()==Some("application/json")) {
        return Err(error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required"));
    }
    match Limited::new(req.into_body(), limit).collect().await {
        Ok(v) => Ok(v.to_bytes()),
        Err(_) => Err(error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large")),
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
        _ => return error(StatusCode::BAD_REQUEST, "unknown_control"),
    };
    let changed = match control::apply_action(&app.control, &command).await {
        Ok(changed) => changed,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "save_failed"),
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
    if slot.busy { return error(StatusCode::CONFLICT, "login_busy"); }
    if path == "/api/auth/start" { return start_login(app, &mut slot).await; }
    let request: LoginCallbackRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return error(StatusCode::BAD_REQUEST, "bad_login_request"),
    };
    let started = take_callback_code(&request.url)
        .and_then(|(code, state)| begin_login(app, &mut slot, code, state));
    drop(slot);
    match started {
        Ok(()) => login_json_reply(wait_for_login(app).await),
        Err(refusal) => refusal.reply(),
    }
}

async fn start_login(app: &Arc<App>, slot: &mut LoginSlot) -> Reply {
    let configured = XboxAuth::new().load_cached_tokens().await.unwrap_or(false);
    if !can_login(configured, &receiver_state(app)) {
        return error(StatusCode::FORBIDDEN, "login_not_needed");
    }
    let attempt = LoginAttempt::new();
    let reply = json!({
        "url": attempt.authorize_url(),
        "loopback": login::uses_loopback(),
        "expires_in": login::ATTEMPT_TTL.as_secs(),
    });
    slot.attempt = Some(attempt);
    slot.last_failure = None;
    json_response(StatusCode::OK, reply)
}

// 登录请求没被接受的原因：status 给接口用；跳回页面按 code 取当前语言的说明，detail 是只用于排查的补充信息
struct Refusal { status: StatusCode, code: &'static str, detail: Option<String> }

impl Refusal {
    fn new(status: StatusCode, code: &'static str) -> Self { Self { status, code, detail: None } }
    fn with_detail(mut self, detail: String) -> Self { self.detail = Some(detail); self }
    // 说明里带上补充信息，直接用 curl 调接口的人也能看到原因
    fn reply(&self) -> Reply {
        let mut extra = json!({});
        if let Some(detail) = &self.detail { extra["detail"] = json!(detail); }
        error_with(self.status, self.code, &self.message(login::Lang::Zh), extra)
    }
    fn message(&self, lang: login::Lang) -> String {
        let Some(detail) = &self.detail else { return text(lang, self.code).to_owned() };
        let (open, close) = match lang { login::Lang::Zh => ("（", "）"), login::Lang::En => (" (", ")") };
        format!("{}{open}{detail}{close}", text(lang, self.code))
    }
}

// 微软在跳转网址里直接带回了错误（例如用户取消了授权）
fn provider_error(reason: String) -> Refusal {
    Refusal::new(StatusCode::BAD_REQUEST, "login_provider_error").with_detail(reason)
}

// 换取或写入凭据失败：标识给管理页翻译，detail 只用于排查
fn login_refusal(status: StatusCode, failure: &LoginFailure) -> Refusal {
    Refusal::new(status, failure_code(failure)).with_detail(failure_detail(failure))
}

fn failure_code(failure: &LoginFailure) -> &'static str {
    match failure { LoginFailure::Exchange(failure) => failure.code, LoginFailure::Store(_) => "store_failed" }
}
fn failure_detail(failure: &LoginFailure) -> String {
    match failure { LoginFailure::Exchange(failure) => failure.detail.clone(), LoginFailure::Store(error) => error.clone() }
}

// 从跳转后的网址里取出授权码和 state。没有授权码的两种情况都算没被接受
fn take_callback_code(raw: &str) -> Result<(String, String), Refusal> {
    match login::parse_callback(raw) {
        Some(ParsedCallback::Code { code, state }) => Ok((code, state)),
        Some(ParsedCallback::Failed(reason)) => Err(provider_error(reason)),
        None => Err(Refusal::new(StatusCode::BAD_REQUEST, "credentials_missing")),
    }
}

// 校验 state 后在后台换取令牌并写入凭据。调用方持有 slot，并保证此刻没有进行中的登录
fn begin_login(app: &Arc<App>, slot: &mut LoginSlot, code: String, state: String) -> Result<(), Refusal> {
    let attempt = match slot.attempt.take() {
        Some(attempt) if !attempt.is_expired() => attempt,
        _ => return Err(Refusal::new(StatusCode::BAD_REQUEST, "expired")),
    };
    if !attempt.matches(&state) {
        return Err(Refusal::new(StatusCode::BAD_REQUEST, "state_mismatch"));
    }
    slot.busy = true;
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let outcome = attempt.complete(&code).await;
        let mut slot = app.login.lock().await;
        slot.busy = false;
        slot.last_failure = outcome.as_ref().err().cloned();
        drop(slot);
        match outcome {
            Ok(()) => { crate::log!("LOGIN ok=true"); restart_streaming(&app).await; }
            Err(e) => crate::log!("LOGIN ok=false reason={e}"),
        }
    });
    Ok(())
}

// 登录成功后，串流开着的话立即重连，让新的凭据马上生效
async fn restart_streaming(app: &App) {
    if !app.control.borrow().enabled { return; }
    let _ = control::apply_action(&app.control, &Command { action: Action::Restart, enabled: None }).await;
}

async fn wait_for_login(app: &App) -> LoginOutcome {
    tokio::select! {
        _ = tokio::time::sleep(LOGIN_WAIT) => {},
        _ = login_finished(app) => {},
    }
    let slot = app.login.lock().await;
    if slot.busy { return LoginOutcome::Pending; }
    match &slot.last_failure {
        Some(failure) => LoginOutcome::Failed(failure.clone()),
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
        LoginOutcome::Failed(failure) => login_refusal(StatusCode::BAD_GATEWAY, &failure).reply(),
    }
}

// 跳回页面只能打开一次：里面的授权码通过校验后就会在后台用掉，刷新时看到的是「已过期」
async fn login_callback_page(query: Option<&str>, app: &Arc<App>, lang: login::Lang) -> Reply {
    let started = match take_callback_code(query.unwrap_or("")) {
        Ok((code, state)) => {
            let mut slot = app.login.lock().await;
            if slot.busy { Err(Refusal::new(StatusCode::CONFLICT, "login_busy")) }
            else { begin_login(app, &mut slot, code, state) }
        }
        Err(refusal) => Err(refusal),
    };
    let (title, lines) = match started {
        Ok(()) => callback_outcome(wait_for_login(app).await, lang),
        Err(refusal) => ("failed_title", vec![refusal.message(lang), text(lang, "failed_body").to_owned()]),
    };
    text_page(lang, text(lang, title), &lines)
}

// 登录跳回页面的标题和正文
fn callback_outcome(outcome: LoginOutcome, lang: login::Lang) -> (&'static str, Vec<String>) {
    match outcome {
        LoginOutcome::Done => ("success_title", vec![text(lang, "success_body").to_owned()]),
        LoginOutcome::Pending => ("pending_title", vec![text(lang, "pending_body").to_owned()]),
        LoginOutcome::Failed(failure) => ("failed_title", vec![login_refusal(StatusCode::BAD_GATEWAY, &failure).message(lang), text(lang, "failed_body").to_owned()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login::ExchangeFailure;

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
        for route in ["/", "/app.js", "/i18n.js", "/style.css"] {
            assert!(files.get(route).is_some_and(|(_, body)| !body.is_empty()), "缺少 {route}");
        }
        let page = String::from_utf8_lossy(&files["/"].1);
        assert!(page.contains("src=\"/i18n.js\" defer></script><script src=\"/app.js\"") && page.contains("href=\"/style.css\""), "页面引用的路径要和路由对得上，文字表要先于脚本加载");
    }

    #[test]
    fn directory_override_replaces_embedded_files() {
        let dir = std::env::temp_dir().join(format!("xsx-dashboard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["index.html", "app.js", "i18n.js", "style.css"] { std::fs::write(dir.join(name), name).unwrap(); }
        let files = load_files(&dir).unwrap();
        assert_eq!(files["/"].1, "index.html");
        assert_eq!(files["/style.css"].0, "text/css; charset=utf-8");
        assert_eq!(files["/i18n.js"].0, "text/javascript; charset=utf-8");
        assert_eq!(&files["/i18n.js"].1[..], b"i18n.js");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_override_fails_loudly_when_a_file_is_missing() {
        let dir = std::env::temp_dir().join(format!("xsx-dashboard-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_files(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // 服务端给出的每个标识都要有中文和英文，漏了的话管理页只会显示空字符串
    #[test]
    fn every_text_has_both_languages() {
        for (key, zh, en) in TEXTS { assert!(!zh.is_empty() && !en.is_empty(), "{key} 缺少说明文字"); }
        assert_eq!(text(login::Lang::Zh, "no_such_key"), "");
    }

    // i18n.js 里每种语言的文字各占一行（键顶格），同一个键出现两次就是中英文都有
    fn has_text_in_both_languages(key: &str) -> bool {
        include_str!("../../dashboard/i18n.js").matches(format!("\n {key}:\"").as_str()).count() == 2
    }
    // 页面上 data-i18n… 属性里的文字键
    fn page_keys(page: &str) -> Vec<&str> {
        page.match_indices("data-i18n").filter_map(|(at, _)| {
            let rest = &page[at..];
            let open = rest.find('"')?;
            let close = open + 1 + rest[open + 1..].find('"')?;
            Some(&rest[open + 1..close])
        }).collect()
    }
    // 脚本里的文字键：直接传给 t() 的，和状态文字表里写成 title:"…" / detail:"…" 的
    fn script_keys(script: &str) -> Vec<&str> {
        script.match_indices('"').filter_map(|(at, _)| {
            let before = script[..at].trim_end();
            let is_call = before.strip_suffix("t(").is_some_and(|rest| rest.chars().next_back().is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$'));
            if !is_call && !before.ends_with("title:") && !before.ends_with("detail:") { return None; }
            let close = at + 1 + script[at + 1..].find('"')?;
            Some(&script[at + 1..close])
        }).collect()
    }

    // 页面和脚本里用到的每个文字键，中英文都要有，否则页面上会直接显示键名
    #[test]
    fn every_text_key_on_the_page_has_both_languages() {
        let page = String::from_utf8_lossy(&embedded_files()["/"].1).into_owned();
        let script = include_str!("../../dashboard/app.js");
        let keys: Vec<&str> = page_keys(&page).into_iter().chain(script_keys(script)).collect();
        assert!(keys.len() > 100, "只找到 {} 个文字键，扫描规则可能失效了", keys.len());
        for key in &keys { assert!(has_text_in_both_languages(key), "i18n.js 里缺少 {key} 的中英文文字"); }
        assert!(!script.contains(" t(`") && !script.contains("(t(`"), "键要写全，不要拼字符串，否则这里核对不到");
    }

    // 接口会返回的每个报错代码，服务端和管理页都要有两种语言的说明
    #[test]
    fn api_error_codes_have_text_on_the_server_and_on_the_page() {
        let source = include_str!("dashboard.rs").split("#[cfg(test)]").next().unwrap();
        let mut codes: Vec<&str> = source.lines()
            .filter(|line| line.contains("error(StatusCode::") || line.contains("Refusal::new(StatusCode::"))
            .filter_map(|line| line.rsplit('"').nth(1)).collect();
        codes.extend(["invalid_grant", "invalid_client", "no_refresh_token", "redirect_mismatch", "rejected", "exchange_error", "store_failed"]);
        assert!(codes.len() >= 20, "只找到 {} 个报错代码，扫描规则可能失效了", codes.len());
        for code in codes {
            assert!(!text(login::Lang::Zh, code).is_empty() && !text(login::Lang::En, code).is_empty(), "{code} 在服务端缺少说明文字");
            assert!(has_text_in_both_languages(code), "{code} 在 i18n.js 里缺少中英文文字");
        }
    }

    #[tokio::test]
    async fn error_replies_carry_the_code() {
        let body = error(StatusCode::NOT_FOUND, "not_found").into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, json!({"error":"页面不存在","code":"not_found"}));
        let refusal = provider_error("access_denied".to_owned());
        let body = refusal.reply().into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, json!({"error":"微软返回了错误（access_denied）","code":"login_provider_error","detail":"access_denied"}));
    }

    #[test]
    fn login_failures_are_reported_by_code_in_both_languages() {
        let exchange = LoginFailure::Exchange(ExchangeFailure { code: "redirect_mismatch", detail: "200 code=invalid_request".to_owned() });
        let refusal = login_refusal(StatusCode::BAD_GATEWAY, &exchange);
        assert_eq!(refusal.message(login::Lang::Zh), "跳回地址与应用里登记的不一致，请检查 AUTH_REDIRECT_URI（200 code=invalid_request）");
        assert_eq!(refusal.message(login::Lang::En), "The redirect address does not match the one registered for the app. Check AUTH_REDIRECT_URI (200 code=invalid_request)");
        assert_eq!(exchange.to_string(), "redirect_mismatch: 200 code=invalid_request");
        let store = LoginFailure::Store("disk full".to_owned());
        assert_eq!(failure_code(&store), "store_failed");
        assert_eq!(failure_detail(&store), "disk full");
        assert_eq!(login_refusal(StatusCode::BAD_GATEWAY, &store).message(login::Lang::Zh), "凭据写入失败（disk full）");
        assert_eq!(Refusal::new(StatusCode::CONFLICT, "login_busy").message(login::Lang::En), "Another sign-in is still in progress, please wait");
        assert_eq!(provider_error("access_denied".to_owned()).message(login::Lang::En), "Microsoft returned an error (access_denied)");
    }

    #[tokio::test]
    async fn callback_pages_follow_the_page_language() {
        let done = callback_outcome(LoginOutcome::Done, login::Lang::En);
        assert_eq!(done, ("success_title", vec!["You can close this page and go back to the dashboard.".to_owned()]));
        let failed = callback_outcome(LoginOutcome::Failed(LoginFailure::Store("disk full".to_owned())), login::Lang::Zh);
        assert_eq!(failed.0, "failed_title");
        assert_eq!(failed.1, vec!["凭据写入失败（disk full）", "请回到串流管理页重新开始。"]);
        let page = String::from_utf8_lossy(&text_page(login::Lang::En, text(login::Lang::En, &done.0), &done.1).into_body().collect().await.unwrap().to_bytes()).into_owned();
        assert!(page.starts_with("<!doctype html><html lang=\"en\">"), "{page}");
        assert!(page.contains("<title>Xbox sign-in successful</title>") && page.contains("Back to the dashboard"), "{page}");
        let zh = String::from_utf8_lossy(&text_page(login::Lang::Zh, "标题", &["第一段".to_owned(), "<b>原样显示</b>".to_owned()]).into_body().collect().await.unwrap().to_bytes()).into_owned();
        assert!(zh.contains("<html lang=\"zh-CN\">") && zh.contains("<p>第一段</p><p>&lt;b&gt;原样显示&lt;/b&gt;</p>"), "{zh}");
    }

    // MEDIA_HTTP_BASE 是默认的回环地址时，播放地址用访问管理页的 IP 生成；显式配置了别的地址则原样使用
    #[test]
    fn media_links_follow_the_access_address_unless_configured() {
        for (configured, expected) in [
            ("http://127.0.0.1:8888", "http://192.0.2.10:8888"),
            ("http://localhost:8888/", "http://192.0.2.10:8888"),
            ("http://127.0.0.1:8888/srs", "http://192.0.2.10:8888/srs"),
            ("http://127.0.0.1", "http://192.0.2.10"),
        ] {
            assert_eq!(media_base(configured, "192.0.2.10:8099"), expected, "{configured}");
        }
        assert_eq!(media_base("http://[::1]:8888", "[::1]:8099"), "http://[::1]:8888");
        assert_eq!(media_base("http://192.0.2.50:8888", "192.0.2.10:8099"), "http://192.0.2.50:8888", "显式配置的地址不替换");
        assert_eq!(media_base("192.0.2.50", "192.0.2.10:8099"), "192.0.2.50", "不是网址的值原样返回");
        assert_eq!(media_base("http://127.0.0.1:8888", ""), "http://127.0.0.1:8888", "拿不到访问地址时不替换");
    }
}
