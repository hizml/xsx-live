// Headless LAN-only encoded-media receiver. Upstream references are pinned in UPSTREAM_COMMIT.
use anyhow::{anyhow,bail,Context,Result};
use xsx_native::log;
use std::{collections::HashMap,net::{IpAddr,SocketAddr,UdpSocket as StdUdpSocket},os::unix::fs::DirBuilderExt,path::{Path,PathBuf},time::{Duration,Instant}};
use str0m::{Rtc,Candidate,Event,Input,Output};
use str0m::channel::{ChannelId,ChannelConfig};
use str0m::change::SdpAnswer;
use str0m::media::{Direction,MediaKind};
use str0m::net::{Protocol,Receive};
use tokio::net::UdpSocket;
use xsx_native::relay::{MediaRelay,Sample};
use xsx_native::{auth::XboxAuth,xhome::XHomeClient,rtc::protocol::{self,InboundMsg}};
// 取出一帧里分发用得上的部分
fn sample_of(data:&str0m::media::MediaData)->Sample<'_>{Sample{data:&data.data,time:data.time.as_micros(),is_contiguous:data.contiguous,is_keyframe:data.is_keyframe()}}
const CHANNELS:[(&str,&str);4]=[("chat","chatV1"),("control","controlV1"),("message","messageV1"),("input","1.0")];
fn uuid()->String{
 let mut b=[0u8;16];getrandom::fill(&mut b).expect("OS randomness");b[6]=(b[6]&15)|64;b[8]=(b[8]&63)|128;
 format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15])
}
fn write_dc(rtc:&mut Rtc,id:Option<ChannelId>,v:&impl serde::Serialize){
 if let Some(id)=id {if let Some(mut ch)=rtc.channel(id) {let _=ch.write(true,&serde_json::to_vec(v).expect("protocol serialization"));}}
}
fn send_gamepad(rtc:&mut Rtc,id:Option<ChannelId>,seq:&mut u32,frame:&xsx_native::rtc::input::GamepadFrame,elapsed:Duration){
 let bytes=xsx_native::rtc::input::encode_gamepad(frame,*seq,elapsed.as_secs_f64()*1000.0);*seq=seq.wrapping_add(1);
 if let Some(id)=id {if let Some(mut ch)=rtc.channel(id) {let _=ch.write(true,&bytes);}}
}
// 会话保活：主机在约 60 秒没收到输入报告时会用 KickForNoClientConnection 结束会话。
// 做法是「打个招呼」——注册手柄 0，发几帧中立输入，再把手柄松开；之后主机就会一直保留
// 这个安静的会话。松开后手柄槽位归还本机，不影响主机上的物理手柄。
fn release_pad(rtc:&mut Rtc,control:Option<ChannelId>){
 let mut removed=protocol::gamepad_changed();removed.was_added=false;write_dc(rtc,control,&removed);
}
// PadPulse 管「动一下虚拟手柄」的两件事：握手后的问候，以及空闲警告后的微脉冲。
// 每个动作都是 注册 → 等一个 tick → 发帧 → 松开，一步一个 tick，做完就把玩家位还给主机
const HELLO_FRAMES:u8=2;
// 空闲保活。主机约 10 分钟收不到任何手柄输入会发 WarningForBeingIdle，120 秒后以 KickForBeingIdle 结束会话。
// 中立输入（什么都没按）不能重置这个计时器；微软官方网页客户端的做法是把左摇杆推到 12.5%、几十毫秒后
// 回中，会话管理器把它算作用户活动。这里照做：收到警告时推一次，半分钟后再推一次作为保险，之后没有新警告就停。
// 所以它只发生在「10 分钟没人碰手柄」之后，主机上有人在玩时不会推摇杆。
// 官方客户端是之后一直每 30 秒重复；这里一轮只推两次，因为每推一次主机画面上会弹一次「正在远程游戏」
const IDLE_REPEAT:Duration=Duration::from_secs(30);
// 警告之后这么长时间之内允许重复推。窗口收得和重复间隔一样长，等于每轮警告只推一次
// （用户定的：提示弹多了烦）。主机给的倒计时是 120 秒，实测推一次就够挡住
const IDLE_WINDOW:Duration=Duration::from_secs(30);
// 挂机保活把虚拟手柄保持注册（整个挂机段只注册一次，不反复进出主机的设备列表）。
// 距上次微脉冲超过这么久还没来新的一轮警告，多半是有人回来玩了（本地输入同样会顶掉警告），
// 这时释放虚拟手柄。新警告的实际间隔约 10 分钟，这里留 3 分钟余量
const IDLE_ROUND_GAP:Duration=Duration::from_secs(780);
const RECENTER_DELAY:Duration=Duration::from_millis(40);
// 串流进行时定期向微软查主机的电源状态。连续两次不是「开着」就结束会话
const POWER_POLL_SECS:u64=30;
const POWER_OFF_CONFIRM:u8=2;
// 记录主机发来的控制消息时，同一种消息只记前几次，之后每分钟记一次
const INBOUND_LOG_FULL_TIMES:u32=3;
const INBOUND_LOG_EVERY:u32=60;
const INBOUND_LOG_CHARS:usize=240;
#[derive(Clone,PartialEq,Debug)]
enum PadStep{None,Pulse}
#[derive(Default)]
struct PadPulse{settle:u8,neutral:u8,pulse:bool,release:bool,parked:bool,warned_at:Option<tokio::time::Instant>,last:Option<tokio::time::Instant>}
impl PadPulse{
 // 握手完成后打个招呼：发两帧中立输入再松开
 fn hello(&mut self){self.neutral=HELLO_FRAMES;self.release=true;}
 fn on_idle_warning(&mut self,rtc:&mut Rtc,control:Option<ChannelId>){
  // 一轮只按第一次警告计时：主机会在几秒内连发几次警告，每次都重置起点会把第二次微脉冲放进窗口
  if self.warned_at.is_none(){self.warned_at=Some(tokio::time::Instant::now());}
  // 距上一次微脉冲不满 30 秒就不再推：120 秒内有一次活动就够，
  // 而频繁推摇杆会干扰主机上正在玩的人。之后的重复交给 in_warning_window
  if self.last.is_some_and(|t|t.elapsed()<IDLE_REPEAT){return;}
  self.reengage(rtc,control);
 }
 // 发微脉冲。手柄没在保持注册时先重新注册并等一个 tick 让注册生效；挂机保活的脉冲推完
 // 不再释放：保持注册到挂机结束（见 IDLE_ROUND_GAP）。开场问候推完照旧立即释放
 fn reengage(&mut self,rtc:&mut Rtc,control:Option<ChannelId>){
  if self.parked{self.pulse=true;return;}
  if !self.release{write_dc(rtc,control,&protocol::gamepad_changed());self.settle=1;}
  self.pulse=true;
 }
 // 距最近一次警告还在倒计时之内、且距上次微脉冲满 30 秒，就再推一次。
 // 倒计时过去仍没有新警告，说明微脉冲起作用了，到此为止
 fn in_warning_window(&mut self)->bool{
  let Some(warned)=self.warned_at else{return false};
  if warned.elapsed()>=IDLE_WINDOW{self.warned_at=None;log!("PAD_IDLE_WINDOW_END");return false;}
  self.last.is_none_or(|t|t.elapsed()>=IDLE_REPEAT)
 }
 fn tick(&mut self,rtc:&mut Rtc,control:Option<ChannelId>,input:Option<ChannelId>,seq:&mut u32,elapsed:Duration)->PadStep{
  if self.settle>0{self.settle-=1;}
  else if self.neutral>0{
   send_gamepad(rtc,input,seq,&xsx_native::rtc::input::GamepadFrame::neutral(),elapsed);
   self.neutral-=1;log!("PAD_HELLO frames_left={}",self.neutral);
  }else if self.pulse{
   send_gamepad(rtc,input,seq,&xsx_native::rtc::input::GamepadFrame::idle_pulse(),elapsed);
   self.pulse=false;self.last=Some(tokio::time::Instant::now());
   if !self.release{self.parked=true;}
   log!("PAD_IDLE_PULSE");return PadStep::Pulse;
  }else if self.release{release_pad(rtc,control);self.release=false;self.parked=false;log!("PAD_RELEASED");}
  else if self.in_warning_window(){self.reengage(rtc,control);}
  else if self.parked&&self.last.is_some_and(|t|t.elapsed()>=IDLE_ROUND_GAP){self.release=true;}
  PadStep::None
 }
}
// 给主机发来的一条消息归类：通道、消息类型、目标路径
fn classify_inbound(label:&str,bytes:&[u8])->String{
 let value=serde_json::from_slice::<serde_json::Value>(bytes).ok();
 let field=|name:&str|value.as_ref().and_then(|v|v.get(name)).and_then(|v|v.as_str()).unwrap_or("-");
 format!("{label}/{}/{}",field("type"),field("target"))
}
fn should_log_inbound(count:u32)->bool{count<=INBOUND_LOG_FULL_TIMES||count%INBOUND_LOG_EVERY==0}
// 把主机发到控制通道和消息通道的内容记进日志，用来找「主机关机」这类状态变化。
// 周期性的消息只抽样记录，免得把日志撑满
fn log_inbound(seen:&mut HashMap<String,u32>,labels:&HashMap<ChannelId,&'static str>,id:ChannelId,bytes:&[u8]){
 let Some(label)=labels.get(&id) else{return};
 let count=seen.entry(classify_inbound(label,bytes)).or_default();*count+=1;
 if !should_log_inbound(*count){return;}
 let head=String::from_utf8_lossy(&bytes[..bytes.len().min(INBOUND_LOG_CHARS)]).replace(['\r','\n']," ");
 log!("DC_IN {} n={count} {head}",classify_inbound(label,bytes));
}
// 主机不在线（关机或待机）时结束会话的原因；这两种都转入每 60 秒查一次的「等待开机」
fn is_console_unavailable(reason:&str)->bool{reason=="CONSOLE_OFFLINE"||reason=="CONSOLE_POWER_OFF"}
// 主机发出 KickForServerShutdown 之后还要几分钟才真正关机，这期间它仍会接受新的串流会话，
// 一连上就又把它拉住。所以收到这条消息后不重连，直到微软那边报告它不是「开着」为止。
// 不设超时：主机没关完时接口会一直报「开着」，按时间放行等于又把它连上。
// 主机确实被重新打开了的话，用户可以在管理页点「重新连接」
const SHUTDOWN_REASON:&str="CONSOLE_SHUTTING_DOWN";
const SHUTDOWN_POLL:Duration=Duration::from_secs(15);
// 主机主动断开时，接收端以什么原因结束会话
fn describe_disconnect(reason:protocol::DisconnectReason)->String{
 match reason{
  protocol::DisconnectReason::Other(other) if other=="KickForServerShutdown"=>SHUTDOWN_REASON.to_owned(),
  other=>format!("Server disconnected: {other:?}"),
 }
}
// 按接收端的规则挑一台「开着」的主机：没找到就是 None
fn powered_on_console<'a>(consoles:&'a [xsx_native::xhome::XHomeConsole],configured:Option<&str>)->Option<&'a xsx_native::xhome::XHomeConsole>{
 consoles.iter().find(|c|c.power_state.eq_ignore_ascii_case("On")&&configured.is_none_or(|id|id==c.server_id))
}
// 名下主机整体的电源状态。没有一台开着时返回 None；查询失败时重新登录并返回原因，这一轮不算关机
async fn console_power_state(client:&mut XHomeClient,configured:Option<&str>)->Option<String>{
 match client.get_consoles().await{
  Ok(consoles) if powered_on_console(&consoles,configured).is_none()=>None,
  Ok(consoles)=>Some(consoles.first().map(|c|c.power_state.as_str()).unwrap_or("Missing").to_owned()),
  Err(e)=>{let _=client.login().await;Some(format!("probe_error={e}"))},
 }
}
// 等主机真正关机：定时查电源状态并写进状态文件，管理页据此显示「主机正在关机」。
// 查到主机不是「开着」、用户改了串流开关、或者服务退出时结束
async fn wait_for_console_off(revision:u64,mut shutdown:tokio::sync::watch::Receiver<bool>,mut control:tokio::sync::watch::Receiver<xsx_native::control::ControlState>){
 let configured=std::env::var("XBOX_SERVER_ID").ok();
 let mut client=XHomeClient::new(XboxAuth::new());let started=Instant::now();let mut polls=tokio::time::interval(SHUTDOWN_POLL);
 loop{
  let should_leave=tokio::select!{
   _=polls.tick()=>false,
   _=shutdown.changed()=>true,
   _=control.changed()=>control.borrow().revision!=revision,
  };
  if should_leave{return;}
  let state=console_power_state(&mut client,configured.as_deref()).await;
  let seconds=started.elapsed().as_secs();
  log!("CONSOLE_SHUTDOWN power_state={} seconds={seconds} finished={}",state.as_deref().unwrap_or("Off"),state.is_none());
  let details=serde_json::json!({"control_revision":revision,"reason":SHUTDOWN_REASON,"power_state":state,"shutdown_seconds":seconds});
  if let Err(e)=xsx_native::health::update("waiting_console",details){log!("HEALTH_UPDATE failed={e}");}
  if state.is_none(){return;}
 }
}
// 等用户改了串流开关（修订号变化）。只改「开机自动串流」这类设置不算，不打断正在进行的连接或串流
async fn wait_for_revision_change(mut control:tokio::sync::watch::Receiver<xsx_native::control::ControlState>,revision:u64){
 while control.changed().await.is_ok(){if control.borrow().revision!=revision{return;}}
}
// 主机自己关机了，不再重连。「开机自动串流」关着就停掉串流，下次开机不自动开始；
// 开着就等主机真正关机，再回到「等待开机」
async fn handle_console_shutdown(control_tx:tokio::sync::watch::Sender<xsx_native::control::ControlState>,control:tokio::sync::watch::Receiver<xsx_native::control::ControlState>,revision:u64,shutdown:tokio::sync::watch::Receiver<bool>){
 if !control.borrow().auto_start{
  let stop=xsx_native::control::Command{action:xsx_native::control::Action::Stop,enabled:None};
  if let Err(e)=xsx_native::control::apply_action(&control_tx,&stop).await{log!("CONTROL_SAVE failed={e}");}
  log!("CONSOLE_SHUTDOWN auto_start=false stream_stopped=true");return;
 }
 wait_for_console_off(revision,shutdown,control).await;
}
// 串流进行时另起一个任务向微软查主机的电源状态，连续 POWER_OFF_CONFIRM 次不是「开着」就把返回的通道置为 true，
// 接收循环每转到一圈看一眼。不放进接收循环里，是因为这个请求最长要等 20 秒，在循环里等会让画面卡住
fn spawn_power_probe(client:XHomeClient,console_id:String)->(tokio::sync::watch::Receiver<bool>,tokio::task::JoinHandle<()>){
 let (off_tx,off_rx)=tokio::sync::watch::channel(false);
 let task=tokio::spawn(async move{
  let mut client=client;let mut misses=0u8;
  loop{
   tokio::time::sleep(Duration::from_secs(POWER_POLL_SECS)).await;
   match client.get_consoles().await{
    Ok(consoles)=>{
     let state=consoles.iter().find(|c|c.server_id==console_id).map(|c|c.power_state.as_str()).unwrap_or("Missing");
     log!("CONSOLE_POWER state={state}");
     misses=if state.eq_ignore_ascii_case("On"){0}else{misses.saturating_add(1)};
     if misses>=POWER_OFF_CONFIRM{let _=off_tx.send(true);return;}
    },
    // 查询失败多半是登录凭据过期，重新登录一次下轮再查；这一轮不计入「不是开着」
    Err(e)=>{log!("CONSOLE_POWER probe_error={e}");if client.login().await.is_err(){tokio::time::sleep(Duration::from_secs(20)).await;}}
   }
  }
 });
 (off_rx,task)
}
// 配置目录里的内容由程序自己准备：凭据目录，以及播放页的文件（设了 WWW_DIR 才写，分发服务从那里读页面）。
// 这样镜像装好、应用建好之后目录会自动生成，不用提前往里放文件。失败只记日志，不耽误启动
fn prepare_config(){
 let auth=std::env::var_os("XBOX_AUTH_FILE").map(PathBuf::from).unwrap_or_else(||PathBuf::from("/auth/session.json"));
 if let Some(dir)=auth.parent(){
  // 凭据目录只让属主进入
  let mut create=std::fs::DirBuilder::new();create.recursive(true);create.mode(0o700);
  if let Err(e)=create.create(dir){log!("AUTH_DIR failed={e}");}
 }
 if let Ok(dir)=std::env::var("WWW_DIR"){
  if let Err(e)=xsx_native::assets::install_www(Path::new(&dir)){log!("WWW_INSTALL failed={e}");}
 }
}
#[tokio::main]
async fn main()->Result<()> {
 tracing_subscriber::fmt().with_ansi(false).with_env_filter("warn").init();
 match std::env::args().nth(1).as_deref() {
  Some("--version")=>{println!("xsx-native {}",xsx_native::VERSION);return Ok(())},
  Some("--healthcheck")=>return xsx_native::health::check(),
  Some("--auth-check")=>{
   let _=XboxAuth::new().get_auth_header().await.context("Authorization check failed")?;
   log!("AUTH_CHECK valid=true");return Ok(());
  },
  Some(_)=>bail!("Unknown option"),None=>{}
 }
 // 日志同时写进状态目录里的 receiver.log
 xsx_native::logging::open(Path::new(&std::env::var("STATE_DIR").unwrap_or_else(|_| "/state".into())));
 // 第一行就写明版本，翻日志时先看这一行，才知道跑的是哪一版
 log!("VERSION version={}",xsx_native::VERSION);
 let (shutdown_tx,mut shutdown)=tokio::sync::watch::channel(false);
 tokio::spawn(async move {
  let mut term=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
  tokio::select!{_=term.recv()=>{},_=tokio::signal::ctrl_c()=>{}}
  let _=shutdown_tx.send(true);
 });
 prepare_config();
 let initial=xsx_native::control::ControlState::load().context("Control state")?;
 let (control_tx,mut control)=tokio::sync::watch::channel(initial);
 // 接收循环自己留一份发送端：主机关机且「开机自动串流」关着时，要由它来停掉串流
 xsx_native::dashboard::start(control_tx.clone()).await?;
 loop {
  if *shutdown.borrow(){break;}
  let desired=control.borrow_and_update().clone();
  if !desired.enabled {
   xsx_native::health::update("paused",serde_json::json!({"control_revision":desired.revision}))?;
   tokio::select!{
    _=tokio::time::sleep(Duration::from_secs(20))=>{},
    _=shutdown.changed()=>{},
    _=control.changed()=>{}
   }
   continue;
  }
  xsx_native::health::update("connecting",serde_json::json!({"control_revision":desired.revision}))?;
  let outcome=run_session(shutdown.clone(),control.clone(),desired.revision).await;
  if *shutdown.borrow(){break;}
  if control.borrow().revision!=desired.revision {continue;}
  // 失败原因写进状态文件，管理页据此区分「凭据失效」和「主机不在线」；截断以免异常页面撑大文件
  // 错误链要完整：anyhow 默认的 Display 只有最外层上下文（例如「session setup」），
  // 真正的 HTTP 状态码和响应体在里面一层，排查时全靠它
  let reason=outcome.as_ref().err().map(|e|format!("{e:#}")).unwrap_or_default();
  if reason==SHUTDOWN_REASON{
   handle_console_shutdown(control_tx.clone(),control.clone(),desired.revision,shutdown.clone()).await;continue;
  }
  let offline=is_console_unavailable(&reason);
  let delay=if offline{60}else{10};
  if !reason.is_empty() {log!("SESSION_RETRY delay_seconds={delay} reason={reason}");}
  xsx_native::health::update(if offline{"waiting_console"}else{"retrying"},serde_json::json!({"retry_seconds":delay,"control_revision":desired.revision,"reason":reason.chars().take(300).collect::<String>()}))?;
  tokio::select!{
   _=tokio::time::sleep(Duration::from_secs(delay))=>{},
   _=shutdown.changed()=>{},
   _=control.changed()=>{}
  }
 }
 xsx_native::health::update("stopped",serde_json::json!({}))?;
 log!("STOPPED");Ok(())
}
// 接收端要连的主机地址和端口。XBOX_IP 设了就以它为准（端口用固定的 9002），现有安装的行为不变；
// 没设时不提前定地址：等微软把主机的连接候选推过来直接采用（真机实测 serverDetails 里是占位假地址，
// 真地址在候选里，见 ICE_REMOTE 日志）
fn console_address(configured:Option<&str>,cfg:&xsx_native::xhome::StreamConfig)->Result<Option<SocketAddr>>{
 let session_details=cfg.console_ipv4_endpoint().map(|(a,p)|format!("{a}:{p}")).unwrap_or_else(||"none".into());
 let Some(value)=configured.map(str::trim).filter(|value|!value.is_empty())else{
  log!("CONSOLE_ADDRESS source=ice_candidates session_details={session_details}");
  return Ok(None);
 };
 let address:IpAddr=value.parse().context("XBOX_IP 要写成一个 IP 地址")?;
 log!("CONSOLE_ADDRESS source=config configured={address}:9002 session_details={session_details}");
 Ok(Some(SocketAddr::new(address,9002)))
}
async fn run_session(mut shutdown:tokio::sync::watch::Receiver<bool>,control:tokio::sync::watch::Receiver<xsx_native::control::ControlState>,revision:u64)->Result<()> {
 let setup=async {
  let auth=XboxAuth::new();if !auth.load_cached_tokens().await.context("Authorization unavailable")?{bail!("AUTH_REQUIRED");}
  let mut client=XHomeClient::new(auth);client.login().await.context("xHome login")?;
  let consoles=client.get_consoles().await.context("console discovery")?;
  let configured=std::env::var("XBOX_SERVER_ID").ok();
  let console=powered_on_console(&consoles,configured.as_deref()).ok_or_else(||anyhow!("CONSOLE_OFFLINE"))?;
  log!("CONSOLE selected=true power_state={}",console.power_state);
  let cfg=client.create_session(&console.server_id,Some(&console.play_path)).await.context("session setup")?;
  Ok::<_,anyhow::Error>((client,cfg,console.server_id.clone()))
 };
 let (client,cfg,console_id)=tokio::select! {
  value=tokio::time::timeout(Duration::from_secs(170),setup)=>value.context("Session setup timed out")??,
  _=shutdown.changed()=>return Ok(()),
  _=wait_for_revision_change(control.clone(),revision)=>return Ok(())
 };
 let pulse_client=client.clone();let pulse_path=cfg.session_path.clone();
 let pulse_seconds=(cfg.keep_alive_pulse_seconds.unwrap_or(30)as u64/2).clamp(10,20);
 let keep_task=tokio::spawn(async move {
  loop {
   tokio::time::sleep(Duration::from_secs(pulse_seconds)).await;
   match pulse_client.send_keepalive(&pulse_path).await {
    Ok(_)=>{},
    Err(e)=>{if e.to_string().contains("400"){log!("API_KEEPALIVE state=not_needed");break;}
     log!("API_KEEPALIVE state=retry");}
   }
  }
 });
 let (console_off,power_task)=spawn_power_probe(client.clone(),console_id);
 let address=console_address(std::env::var("XBOX_IP").ok().as_deref(),&cfg);
 let result=match address{
  Ok(console)=>tokio::select! {
   value=receive(&client,&cfg.session_path,revision,console_off,console.clone())=>value,
   _=shutdown.changed()=>{log!("STOP_REQUESTED");Ok(())},
   _=wait_for_revision_change(control.clone(),revision)=>{log!("CONTROL_SESSION_END");Ok(())}
  },
  Err(error)=>Err(error),
 };
 keep_task.abort();let _=keep_task.await;power_task.abort();let _=power_task.await;
 match tokio::time::timeout(Duration::from_secs(8),client.delete_session(&cfg.session_path)).await{
  Ok(Ok(_))=>log!("SESSION_CLOSED"),_=>log!("SESSION_CLOSE pending_expiry=true")
 }
 result
}
async fn receive(client:&XHomeClient,path:&str,revision:u64,power_off:tokio::sync::watch::Receiver<bool>,console:Option<SocketAddr>)->Result<()>{
 // 选网卡用的探测：对 UDP 调 connect 不会真发包，只是让系统按路由选一张网卡。
// 自动模式（没配 XBOX_IP）不知道主机地址，用公网地址选默认路由的那张；配置了就直接对主机选
 let probe_target=console.unwrap_or_else(||"8.8.8.8:80".parse().expect("常量地址"));
 let probe=StdUdpSocket::bind("0.0.0.0:0")?;probe.connect(probe_target)?;let local_ip=probe.local_addr()?.ip();drop(probe);
 // 主机的地址：配置了就有；自动模式从收到的第一条主机候选里学到
 let mut peer_ip:Option<IpAddr>=console.map(|console|console.ip());
 let udp=UdpSocket::bind(SocketAddr::new(local_ip,0)).await?;let local=udp.local_addr()?;
 let mut rtc=Rtc::new(Instant::now());rtc.add_local_candidate(Candidate::host(local,"udp").map_err(|e|anyhow!("{e}"))?);
 let mut control=None;let mut message=None;let mut input=None;
 let (video_mid,offer,pending)={
  let mut change=rtc.sdp_api();
  for(label,proto)in CHANNELS{
   let id=change.add_channel_with_config(ChannelConfig{label:label.into(),ordered:true,protocol:proto.into(),negotiated:None,..Default::default()});
   if label=="control"{control=Some(id)}if label=="message"{message=Some(id)}if label=="input"{input=Some(id)}
  }
  let mid=change.add_media(MediaKind::Video,Direction::RecvOnly,None,None,None);
  change.add_media(MediaKind::Audio,Direction::SendRecv,None,None,None);
  let(offer,p)=change.apply().ok_or_else(||anyhow!("No SDP offer"))?;(mid,offer.to_sdp_string(),p)
 };
 // 记录主机发来的控制消息时，用来标明是哪个通道
 let mut labels:HashMap<ChannelId,&'static str>=HashMap::new();
 for(id,label)in[(control,"control"),(message,"message")]{if let Some(id)=id{labels.insert(id,label);}}
 let answer=client.exchange_sdp_offer(path,&offer).await.context("SDP exchange")?;
 log!("SDP dtls={} sdes={}",answer.contains("a=fingerprint:"),answer.contains("a=crypto:"));
 let filtered=answer.lines().filter(|l|!l.starts_with("a=candidate:")).collect::<Vec<_>>().join("\r\n")+"\r\n";
 rtc.sdp_api().accept_answer(pending,SdpAnswer::from_sdp_string(&filtered).map_err(|e|anyhow!("{e}"))?).map_err(|e|anyhow!("{e}"))?;
 // 配置了 XBOX_IP 时手动补一条主机候选；自动模式不补，等微软推来真地址
 let direct=console.map(|console|format!("candidate:1 1 UDP 2130706431 {} {} typ host",console.ip(),console.port()));
 if let Some(direct)=&direct{rtc.add_remote_candidate(Candidate::from_sdp_string(direct).map_err(|e|anyhow!("{e}"))?);}
 for c in offer.lines().filter_map(|l|l.strip_prefix("a=")).filter(|l|l.starts_with("candidate:")){let _=client.send_ice_candidate(path,c).await;}
 log!("LAN_ONLY peer={} local={}",direct.as_ref().map(|d|d.split_whitespace().nth(4).unwrap_or("?").to_owned()).unwrap_or_else(||"ice_candidates".into()),local.ip());
 let mut relay=match std::env::var("OUTPUT_RTMP"){Ok(url)=>Some(MediaRelay::new(&url)?),Err(_)=>None};
 let mut last_video=Instant::now();let mut input_seq=0u32;let mut last_keyframe=Instant::now();
 let idle_keepalive_enabled=std::env::var("IDLE_KEEPALIVE").map(|v|v.trim()!="0").unwrap_or(true);
 let mut pad=PadPulse::default();let mut recenter_at:Option<Instant>=None;let mut seen:HashMap<String,u32>=HashMap::new();
 let started=Instant::now();let mut vf=0u64;let mut af=0u64;let mut vb=0u64;let mut ab=0u64;let mut keyframes=0u64;
 let mut connected=false;let mut handshake=false;let mut buf=vec![0u8;65536];
 // 循环偶尔被卡住时，错过的 tick 不补发，否则手柄动作的每一步会被挤在一起
 let mut tick=tokio::time::interval(Duration::from_millis(500));tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
 let mut last_print=Instant::now();
 loop{
  let deadline=loop{
   match rtc.poll_output().map_err(|e|anyhow!("{e}"))?{
    Output::Timeout(t)=>break t,
    Output::Transmit(t)=>{if peer_ip.is_none_or(|peer|t.destination.ip()==peer){let _=udp.send_to(&t.contents,t.destination).await;}}
    Output::Event(ev)=>match ev{
     Event::Connected=>{connected=true;log!("WEBRTC_CONNECTED seconds={:.2}",started.elapsed().as_secs_f64());}
     Event::IceConnectionStateChange(s)=>log!("ICE {:?}",s),
     Event::ChannelOpen(id,label)=>{log!("CHANNEL {}",label);if label=="message"{write_dc(&mut rtc,Some(id),&protocol::Handshake::new());}}
     Event::ChannelData(data)=>{
      log_inbound(&mut seen,&labels,data.id,&data.data);
      match protocol::parse_message(&data.data){
      InboundMsg::HandshakeAck if !handshake=>{handshake=true;log!("XBOX_HANDSHAKE_ACK");write_dc(&mut rtc,control,&protocol::control_authorization());write_dc(&mut rtc,control,&protocol::gamepad_changed());
       let bytes=xsx_native::rtc::input::encode_client_metadata(input_seq,started.elapsed().as_secs_f64()*1000.0);input_seq=input_seq.wrapping_add(1);if let Some(id)=input {if let Some(mut ch)=rtc.channel(id){let _=ch.write(true,&bytes);}}log!("CLIENT_METADATA_SENT_AFTER_AUTH");
       for m in protocol::config_messages(uuid){write_dc(&mut rtc,message,&m);}write_dc(&mut rtc,control,&protocol::keyframe_request());
       pad.hello();}
      InboundMsg::ServerDisconnect(protocol::DisconnectReason::WarningForBeingIdle{seconds_until_kick})=>{
       log!("IDLE_WARNING seconds_until_kick={seconds_until_kick} keepalive={idle_keepalive_enabled}");
       if idle_keepalive_enabled{pad.on_idle_warning(&mut rtc,control);}
      }
      InboundMsg::ServerDisconnect(reason)=>bail!("{}",describe_disconnect(reason)),_=>{}
     }},
     Event::MediaData(data)=>{
      if data.mid==video_mid{
       if vf==0{log!("VIDEO_CODEC {:?}",data.params.spec().codec);}
       vf+=1;vb+=data.data.len()as u64;if data.is_keyframe(){keyframes+=1;}
       last_video=Instant::now();
       if let Some(out)=relay.as_mut(){out.video(sample_of(&data))?;}
      }else{if af==0{log!("AUDIO_CODEC {:?}",data.params.spec().codec);}af+=1;ab+=data.data.len()as u64;if let Some(out)=relay.as_mut(){out.audio(sample_of(&data))?;}}
     },_=>{}
    }
   }
  };
  if last_print.elapsed()>Duration::from_secs(5){
   log!("MEDIA video_frames={} audio_packets={} video_bytes={} audio_bytes={} keyframes={}",vf,af,vb,ab,keyframes);
   xsx_native::health::update(if vf>0&&af>0{"streaming"}else{"connecting"},serde_json::json!({"control_revision":revision,"video_frames":vf,"audio_packets":af,"video_bytes":vb,"audio_bytes":ab,"keyframes":keyframes,"elapsed_seconds":started.elapsed().as_secs(),"lan_peer":peer_ip.map(|peer|peer.to_string()).unwrap_or_else(||"pending".into())}))?;
   last_print=Instant::now();
  }
  if *power_off.borrow(){bail!("CONSOLE_POWER_OFF");}
  if !rtc.is_alive(){bail!("WebRTC connection closed");}
  if !connected&&started.elapsed()>Duration::from_secs(25){bail!("LAN connection timed out");}
  if connected&&last_video.elapsed()>Duration::from_secs(20){bail!("Video receive watchdog timed out");}
  tokio::select!{
   r=udp.recv_from(&mut buf)=>{let(n,src)=r?;if peer_ip.is_none_or(|peer|src.ip()==peer){let input=Receive::new(Protocol::Udp,src,local,&buf[..n]).map_err(|e|anyhow!("{e}"))?;rtc.handle_input(Input::Receive(Instant::now(),input)).map_err(|e|anyhow!("{e}"))?;}}
   // 微脉冲只保持一小会儿就回中，不等下一个 tick
   _=tokio::time::sleep(recenter_at.unwrap_or_else(Instant::now).saturating_duration_since(Instant::now())),if recenter_at.is_some()=>{
    send_gamepad(&mut rtc,input,&mut input_seq,&xsx_native::rtc::input::GamepadFrame::neutral(),started.elapsed());
    recenter_at=None;log!("PAD_RECENTER");
   }
   _=tokio::time::sleep(deadline.saturating_duration_since(Instant::now()))=>{rtc.handle_input(Input::Timeout(Instant::now())).map_err(|e|anyhow!("{e}"))?;}
   _=tick.tick()=>{
    if let Some(out)=relay.as_mut(){out.tick()?;}
    if !connected{if let Ok(candidates)=client.poll_ice_candidates(path).await{for c in candidates{let parts=c.candidate.split_whitespace().collect::<Vec<_>>();
     // 配置了 XBOX_IP：只认和它一致的候选。自动模式：认「主机型 + 私有 IPv4 + 不是本机自己」的候选
     let wanted=match console{Some(console)=>parts.get(4).is_some_and(|v|*v==console.ip().to_string()),
      None=>parts.get(7)==Some(&"host")&&parts.get(4).and_then(|v|v.parse::<std::net::Ipv4Addr>().ok()).is_some_and(|a|a.is_private()&&a.to_string()!=local_ip.to_string())};
     if wanted{let sample:String=c.candidate.chars().take(120).collect();let key=format!("ice_add_{}",parts.get(4).unwrap_or(&"?"));if!seen.contains_key(&key){seen.insert(key,1);log!("ICE_REMOTE accepted={sample}");}if let Some(addr)=parts.get(4).and_then(|v|v.parse::<IpAddr>().ok()){peer_ip=Some(addr);}if let Ok(parsed)=Candidate::from_sdp_string(&c.candidate){rtc.add_remote_candidate(parsed);}}
     else if !seen.contains_key("ice_remote"){seen.insert("ice_remote".into(),1);let sample:String=c.candidate.chars().take(120).collect();log!("ICE_REMOTE skipped={sample}");}}}}
    if handshake&&recenter_at.is_none(){
     if last_keyframe.elapsed()>Duration::from_secs(2){write_dc(&mut rtc,control,&protocol::keyframe_request());last_keyframe=Instant::now();}
     if pad.tick(&mut rtc,control,input,&mut input_seq,started.elapsed())==PadStep::Pulse{recenter_at=Some(Instant::now()+RECENTER_DELAY);}
    }
   }
  }
 }
}
#[cfg(test)]
mod tests {
 use super::*;
 // 一次「注册 → 等待 → 发帧 → 松开」占用的 tick 数
 const ACTION_TICKS:usize=4;
 fn advance(pad:&mut PadPulse,ticks:usize)->Vec<PadStep>{
  let mut rtc=Rtc::new(Instant::now());let mut seq=0u32;
  (0..ticks).map(|_|pad.tick(&mut rtc,None,None,&mut seq,Duration::ZERO)).collect()
 }
 #[test]
 fn both_console_offline_markers_poll_slowly(){
  assert!(is_console_unavailable("CONSOLE_OFFLINE"));
  assert!(is_console_unavailable("CONSOLE_POWER_OFF"));
  assert!(!is_console_unavailable("Server disconnected: KickForBeingIdle"));
  assert!(!is_console_unavailable(""));
 }
 #[test]
 fn server_shutdown_ends_the_session_with_its_own_marker(){
  use protocol::DisconnectReason::{KickForBeingIdle,Other};
  assert_eq!(describe_disconnect(Other("KickForServerShutdown".to_owned())),SHUTDOWN_REASON);
  assert_eq!(describe_disconnect(Other("KickForBeingIdle".to_owned())),"Server disconnected: Other(\"KickForBeingIdle\")");
  assert_eq!(describe_disconnect(KickForBeingIdle),"Server disconnected: KickForBeingIdle");
 }
 #[tokio::test]
 async fn only_a_revision_change_ends_the_wait(){
  use xsx_native::control::{Action,ControlState};
  let (tx,rx)=tokio::sync::watch::channel(ControlState::default());
  let waiter=tokio::spawn(wait_for_revision_change(rx,0));
  tx.send_replace(ControlState::default().with_auto_start(false));
  assert!(!waiter.is_finished(),"只改设置不应该结束等待");
  tx.send_replace(ControlState::default().apply(Action::Stop));
  tokio::time::timeout(Duration::from_secs(2),waiter).await.expect("改了开关应该结束等待").unwrap();
 }
 #[test]
 fn only_a_powered_on_console_is_selected(){
  let console=|id:&str,power:&str|xsx_native::xhome::XHomeConsole{server_id:id.to_owned(),device_name:String::new(),console_type:String::new(),power_state:power.to_owned(),is_dev_kit:false,play_path:String::new()};
  let consoles=vec![console("a","Off"),console("b","On")];
  assert_eq!(powered_on_console(&consoles,None).map(|c|c.server_id.as_str()),Some("b"));
  assert!(powered_on_console(&consoles,Some("a")).is_none(),"指定了主机的 ID 时只认那一台");
  assert!(powered_on_console(&[console("a","Off")],None).is_none(),"都关着时就是 None");
 }
 #[test]
 fn pulse_frame_deflects_only_the_left_stick(){
  let frame=xsx_native::rtc::input::GamepadFrame::idle_pulse();
  assert_eq!(frame.left_thumb_x,xsx_native::rtc::input::IDLE_PULSE_LEFT_THUMB_X);
  assert_eq!(frame.left_thumb_y,0);assert_eq!(frame.right_thumb_x,0);assert_eq!(frame.right_thumb_y,0);
  assert_eq!(frame.buttons,0);assert_eq!(frame.left_trigger,0);assert_eq!(frame.right_trigger,0);
 }
 #[test]
 fn hello_sends_two_frames_then_gives_the_pad_back(){
  let mut pad=PadPulse::default();pad.hello();
  assert_eq!(advance(&mut pad,2),vec![PadStep::None,PadStep::None]);
  assert!(pad.release,"发完两帧中立输入后手柄还占着");
  advance(&mut pad,1);
  assert!(!pad.release,"第三个 tick 应该松开手柄");
 }
 #[test]
 fn idle_warning_pulses_once_then_gives_the_pad_back(){
  let mut pad=PadPulse::default();let mut rtc=Rtc::new(Instant::now());
  pad.on_idle_warning(&mut rtc,None);
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::None,PadStep::Pulse,PadStep::None,PadStep::None]);
  assert!(!pad.release,"微脉冲发完后手柄应该已经松开");
 }
 #[tokio::test(start_paused=true)]
 async fn pulses_once_per_warning_round(){
  let mut pad=PadPulse::default();let mut rtc=Rtc::new(Instant::now());
  // 主机会在几秒内连发几次警告，一轮里只有第一次才立计时起点
  pad.on_idle_warning(&mut rtc,None);
  tokio::time::advance(Duration::from_secs(1)).await;
  pad.on_idle_warning(&mut rtc,None);
  tokio::time::advance(Duration::from_secs(1)).await;
  pad.on_idle_warning(&mut rtc,None);
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::None,PadStep::Pulse,PadStep::None,PadStep::None]);
  assert!(!pad.release);
  assert!(pad.parked,"挂机期间保持注册，不反复进出设备列表");
  // 过了重复间隔也不再有第二次：窗口按第一次警告算，已经结束
  tokio::time::advance(Duration::from_secs(31)).await;
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::None;ACTION_TICKS]);
  assert_eq!(pad.warned_at,None,"窗口已结束");
  // 之后哪怕再过很久也不动，手柄仍保持注册
  tokio::time::advance(Duration::from_secs(600)).await;
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::None;ACTION_TICKS]);
  assert!(pad.parked);
 }
 #[tokio::test(start_paused=true)]
 async fn a_new_warning_starts_a_new_round(){
  let mut pad=PadPulse::default();let mut rtc=Rtc::new(Instant::now());
  pad.on_idle_warning(&mut rtc,None);advance(&mut pad,ACTION_TICKS);
  tokio::time::advance(IDLE_WINDOW).await;advance(&mut pad,ACTION_TICKS);
  pad.on_idle_warning(&mut rtc,None);
  assert!(pad.parked,"挂机中，新一轮不用重新注册");
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::Pulse,PadStep::None,PadStep::None,PadStep::None],"保持注册，直接推");
 }

 #[tokio::test(start_paused=true)]
 async fn releases_the_parked_pad_when_rounds_stop_coming(){
  let mut pad=PadPulse::default();let mut rtc=Rtc::new(Instant::now());
  pad.on_idle_warning(&mut rtc,None);advance(&mut pad,ACTION_TICKS);
  assert!(pad.parked);
  // 超过一轮的间隔没有新警告（多半是有人回来玩了）：释放，把设备位还回去
  tokio::time::advance(IDLE_ROUND_GAP+Duration::from_secs(1)).await;
  assert_eq!(advance(&mut pad,2),vec![PadStep::None,PadStep::None]);
  assert!(!pad.parked&&!pad.release,"已释放");
 }
 #[tokio::test(start_paused=true)]
 async fn warnings_in_quick_succession_pulse_only_once(){
  let mut pad=PadPulse::default();let mut rtc=Rtc::new(Instant::now());
  pad.on_idle_warning(&mut rtc,None);advance(&mut pad,ACTION_TICKS);
  // 主机几秒内又发了两次警告：不重复推
  tokio::time::advance(Duration::from_secs(2)).await;
  pad.on_idle_warning(&mut rtc,None);pad.on_idle_warning(&mut rtc,None);
  assert_eq!(advance(&mut pad,3),vec![PadStep::None,PadStep::None,PadStep::None]);
  // 满 30 秒后这一轮已经结束：每轮只推一次，不再推
  tokio::time::advance(Duration::from_secs(29)).await;
  assert_eq!(advance(&mut pad,ACTION_TICKS),vec![PadStep::None;ACTION_TICKS]);
  assert_eq!(pad.warned_at,None);
 }
 #[test]
 fn inbound_messages_are_grouped_by_type_and_target(){
  let msg=br#"{"type":"Message","target":"/a/serverInitiatedDisconnect/x","content":"{}"}"#;
  assert_eq!(classify_inbound("control",msg),"control/Message//a/serverInitiatedDisconnect/x");
  assert_eq!(classify_inbound("message",br#"{"type":"HandshakeAck"}"#),"message/HandshakeAck/-");
  assert_eq!(classify_inbound("message",b"\x00\x01\xff"),"message/-/-");
 }
 #[test]
 fn periodic_inbound_messages_are_sampled(){
  assert!(should_log_inbound(1));assert!(should_log_inbound(INBOUND_LOG_FULL_TIMES));
  assert!(!should_log_inbound(INBOUND_LOG_FULL_TIMES+1));
  assert!(should_log_inbound(INBOUND_LOG_EVERY));
 }

    fn session_with_console(ipv4:Option<&str>,port:Option<u16>)->xsx_native::xhome::StreamConfig{
        let details=xsx_native::xhome::ServerDetails{ip_address:None,ipv4_address:ipv4.map(str::to_owned),ipv4_port:port,ipv6_address:None,ipv6_port:None,port:None,tcp_port:None,udp_port:None,sdp:None};
        xsx_native::xhome::StreamConfig{session_id:String::new(),session_path:String::new(),exchange_response:String::new(),gs_token:String::new(),keep_alive_pulse_seconds:None,server_details:Some(details)}
    }
    // XBOX_IP 设了就以它为准（端口固定 9002），不受会话回应影响：现有安装的行为不变
    #[test]
    fn configured_console_address_wins(){
        let session=session_with_console(Some("192.168.1.10"),Some(9004));
        let expected:SocketAddr="192.168.1.180:9002".parse().unwrap();
        assert_eq!(console_address(Some("192.168.1.180"),&session).unwrap(),Some(expected));
        assert!(console_address(Some("not-an-ip"),&session).unwrap_err().to_string().contains("XBOX_IP"));
        assert_eq!(console_address(Some("  "),&session).unwrap(),None,"只有空白等同于没设，走自动模式");
    }
    // 没设 XBOX_IP 时不提前定地址（返回 None），建连接时听微软推来的候选；serverDetails 只当诊断参考
    #[test]
    fn auto_mode_defers_to_ice_candidates(){
        for details in [Some(("192.168.1.10",Some(9004))),Some(("10.0.0.1",Some(0))),None]{
            let (ipv4,port)=details.map(|(a,p)|(Some(a),p)).unwrap_or((None,None));
            assert_eq!(console_address(None,&session_with_console(ipv4,port)).unwrap(),None);
        }
    }
    // serverDetails 里端口为 0 的是占位假地址，不算数
    #[test]
    fn placeholder_session_details_are_ignored(){
        assert_eq!(session_with_console(Some("10.0.0.1"),Some(0)).console_ipv4_endpoint(),None);
        assert_eq!(session_with_console(Some("192.168.1.10"),Some(9004)).console_ipv4_endpoint(),Some(("192.168.1.10".parse().unwrap(),9004)));
        assert_eq!(session_with_console(Some("203.0.113.5"),Some(9002)).console_ipv4_endpoint(),None,"公网地址不算");
    }
}
