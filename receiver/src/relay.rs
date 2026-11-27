use anyhow::{Result,bail};
use std::{path::PathBuf,process::{Child,ChildStdin,Command,Stdio},time::Instant,fs::OpenOptions,io::Write,collections::VecDeque,os::unix::fs::OpenOptionsExt};
// 一帧音视频数据：从 WebRTC 收到的数据里取出分发用得上的部分
#[derive(Clone,Copy)]
pub struct Sample<'a>{pub data:&'a [u8],pub time:u64,pub is_contiguous:bool,pub is_keyframe:bool}
fn id(n:u32)->Vec<u8>{let a=n.to_be_bytes();a[a.iter().position(|v|*v!=0).unwrap_or(3)..].to_vec()}
fn size(n:usize)->Vec<u8>{for width in 1..=8{if (n as u64)<(1u64<<(7*width))-1{let v=(1u64<<(7*width))|n as u64;return v.to_be_bytes()[8-width..].to_vec();}}unreachable!()}
fn element(tag:u32,data:&[u8])->Vec<u8>{let mut b=id(tag);b.extend(size(data.len()));b.extend_from_slice(data);b}
fn uint(tag:u32,n:u64)->Vec<u8>{let a=n.to_be_bytes();element(tag,&a[a.iter().position(|v|*v!=0).unwrap_or(7)..])}
fn text(tag:u32,s:&str)->Vec<u8>{element(tag,s.as_bytes())}
fn join(v:&[Vec<u8>])->Vec<u8>{v.concat()}
fn nals(b:&[u8])->Vec<&[u8]>{
 let mut starts=vec![];let mut i=0;while i+3<=b.len(){if i+4<=b.len()&&b[i..i+4]==[0,0,0,1]{starts.push((i,i+4));i+=4;}else if b[i..i+3]==[0,0,1]{starts.push((i,i+3));i+=3;}else{i+=1;}}
 if starts.is_empty(){return if b.is_empty(){vec![]}else{vec![b]};}
 starts.iter().enumerate().filter_map(|(j,(_,from))|{let to=starts.get(j+1).map(|v|v.0).unwrap_or(b.len());if *from<to{Some(&b[*from..to])}else{None}}).collect()
}
struct Bits{b:Vec<u8>,i:usize}
impl Bits{
 fn bit(&mut self)->Result<u32>{if self.i>=self.b.len()*8{bail!("Truncated H264 SPS");}let v=(self.b[self.i/8]>>(7-self.i%8))&1;self.i+=1;Ok(v as u32)}
 fn n(&mut self,n:usize)->Result<u32>{let mut v=0;for _ in 0..n{v=(v<<1)|self.bit()?;}Ok(v)}
 fn ue(&mut self)->Result<u32>{let mut zeros=0;while self.bit()?==0{zeros+=1;if zeros>30{bail!("Invalid SPS integer");}}Ok(((1u32<<zeros)-1)+self.n(zeros)?)}
 fn se(&mut self)->Result<i32>{let n=self.ue()?;Ok(if n%2==1{((n+1)/2)as i32}else{-((n/2)as i32)})}
}
fn dimensions(s:&[u8])->Result<(u32,u32)>{
 let mut b=Vec::new();let mut zero=0;for &v in &s[1..]{if zero>=2&&v==3{zero=0;continue;}b.push(v);zero=if v==0{zero+1}else{0};}
 let mut r=Bits{b,i:0};let profile=r.n(8)?;r.n(16)?;r.ue()?;let mut chroma=1;
 if [100,110,122,244,44,83,86,118,128,138,139,134,135].contains(&profile){
  chroma=r.ue()?;if chroma==3&&r.bit()?==1{chroma=0;}r.ue()?;r.ue()?;r.bit()?;
  if r.bit()?==1{for index in 0..if chroma==3{12}else{8}{if r.bit()?==1{let mut last=8i32;let mut next=8i32;for _ in 0..if index<6{16}else{64}{if next!=0{next=(last+r.se()?+256)%256;}if next!=0{last=next;}}}}}
 }
 r.ue()?;let poc=r.ue()?;if poc==0{r.ue()?;}else if poc==1{r.bit()?;r.se()?;r.se()?;let count=r.ue()?;if count>255{bail!("Invalid SPS cycle");}for _ in 0..count{r.se()?;}}
 r.ue()?;r.bit()?;let w=(r.ue()?+1)*16;let hm=r.ue()?+1;let frame=r.bit()?;if frame==0{r.bit()?;}r.bit()?;let(mut left,mut right,mut top,mut bottom)=(0,0,0,0);
 if r.bit()?==1{left=r.ue()?;right=r.ue()?;top=r.ue()?;bottom=r.ue()?;}
 let cx=if chroma==1||chroma==2{2}else{1};let cy=if chroma==1{2*(2-frame)}else{2-frame};
 let width=w.saturating_sub((left+right)*cx);let height=((2-frame)*hm*16).saturating_sub((top+bottom)*cy);
 if width==0||height==0||width>8192||height>8192{bail!("Invalid source dimensions");}Ok((width,height))
}
// 源时间戳倒退超过这么多就认为主机重新配置了编码器；网络乱序造成的倒退只有几毫秒
const SOURCE_REWIND_US:u64=1_000_000;
#[derive(Default)]
struct Clock{first:Option<(u64,u64)>,last:Option<(u64,u64)>}
impl Clock{
 // 把源时间戳换算成输出时间戳（毫秒）。主机重新配置编码器后源时间戳可能从头开始，
 // 这时重新对齐，让输出时间戳保持单调递增，画面和声音不会错位
 fn timestamp(&mut self,source:u64,elapsed:u64)->u64{
  let(src,out)=*self.first.get_or_insert((source,elapsed));
  let mut stamp=out.saturating_add(source.saturating_sub(src));
  if self.last.is_some_and(|(last_in,_)|source.saturating_add(SOURCE_REWIND_US)<last_in){
   stamp=self.last.map_or(stamp,|(_,last_out)|last_out+1000);
   self.first=Some((source,stamp));
  }
  self.last=Some((source,stamp));
  stamp/1000
 }
}
// 新旧两份 SPS 各自的画面尺寸
fn sps_sizes(current:&[u8],incoming:&[u8])->Result<((u32,u32),(u32,u32))>{Ok((dimensions(current)?,dimensions(incoming)?))}
fn hex(bytes:&[u8])->String{bytes.iter().map(|b|format!("{b:02x}")).collect()}
pub struct MediaRelay{
 child:Child,pipe:Option<ChildStdin>,started:Instant,video_clock:Clock,audio_clock:Clock,sps:Vec<u8>,pps:Vec<u8>,ready:bool,await_key:bool,cluster:Option<u64>,queued_audio:VecDeque<(u64,Vec<u8>)>,pub video_frames:u64,pub audio_packets:u64
}
 // 一帧视频要不要写进输出
 #[derive(Debug,PartialEq)]
 enum FrameDecision{Write,WaitForKey}
 // 帧不完整（中间丢了 UDP 包）就一律不写，关键帧也不例外：残缺的关键帧一旦写出去，
 // 后面依赖它的一整组帧都会解码报错，播放端表现为卡顿和花屏。丢过帧之后，等下一个完整的关键帧再继续
 fn frame_decision(is_contiguous:bool,is_key:bool,awaiting_key:bool)->FrameDecision{
  if !is_contiguous||awaiting_key&&!is_key{FrameDecision::WaitForKey}else{FrameDecision::Write}
 }
impl MediaRelay{
 pub fn new(url:&str)->Result<Self>{
  let dir=PathBuf::from(std::env::var("STATE_DIR").unwrap_or_else(|_|"/state".into()));std::fs::create_dir_all(&dir)?;
  let log=dir.join("ffmpeg.log");if log.metadata().map(|m|m.len()>2*1024*1024).unwrap_or(false){let _=std::fs::rename(&log,dir.join("ffmpeg.previous.log"));}
  let err=OpenOptions::new().create(true).append(true).mode(0o600).open(log)?;
  let mut child=Command::new("/usr/bin/ffmpeg").args(["-hide_banner","-nostdin","-loglevel","warning","-fflags","+genpts+discardcorrupt","-analyzeduration","1000000","-probesize","2000000","-f","matroska","-i","pipe:0","-map","0:v:0","-map","0:a:0","-c:v","copy","-c:a","aac","-ar","48000","-ac","2","-b:a","160k","-af","aresample=async=1:first_pts=0","-max_interleave_delta","1000000","-flvflags","no_duration_filesize","-f","flv","-rw_timeout","8000000"]).arg(url).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(err).spawn()?;
  let pipe=Some(child.stdin.take().expect("piped FFmpeg input"));
  Ok(Self{child,pipe,started:Instant::now(),video_clock:Clock::default(),audio_clock:Clock::default(),sps:vec![],pps:vec![],ready:false,await_key:true,cluster:None,queued_audio:VecDeque::new(),video_frames:0,audio_packets:0})
 }
 fn pipe(&mut self)->&mut ChildStdin{self.pipe.as_mut().expect("ffmpeg input is open")}
 // 主机在开播一两秒后会调整一次编码参数，运行中也可能再调（例如长时间没人操作时屏幕变暗），
 // 每次都会重新发一份 SPS。这里一律接着写：新 SPS 就在这一帧里，播放端会自己更新，串流不会断。
 // 新 SPS 解析不出来才重连
 fn check_sps_change(&self,incoming:&[u8])->Result<()>{
  if self.sps==*incoming{return Ok(());}
  let ((before_width,before_height),(after_width,after_height))=sps_sizes(&self.sps,incoming)?;
  crate::log!("VIDEO_SPS_CHANGED frames={} size={before_width}x{before_height}->{after_width}x{after_height} before={} after={}",self.video_frames,hex(&self.sps),hex(incoming));
  Ok(())
 }
 fn header(&mut self)->Result<()>{
  let(w,h)=dimensions(&self.sps)?;
  let ebml=element(0x1A45DFA3,&join(&[uint(0x4286,1),uint(0x42F7,1),uint(0x42F2,4),uint(0x42F3,8),text(0x4282,"matroska"),uint(0x4287,4),uint(0x4285,2)]));
  self.pipe().write_all(&ebml)?;self.pipe().write_all(&id(0x18538067))?;self.pipe().write_all(&[1,255,255,255,255,255,255,255])?;
  self.pipe().write_all(&element(0x1549A966,&join(&[uint(0x2AD7B1,1_000_000),text(0x4D80,"xsx-native"),text(0x5741,"xsx-native") ])))?;
  if self.sps.len()<4{bail!("H264 configuration too short");}
  let mut avcc=vec![1,self.sps[1],self.sps[2],self.sps[3],255,225];avcc.extend_from_slice(&(self.sps.len()as u16).to_be_bytes());avcc.extend_from_slice(&self.sps);avcc.push(1);avcc.extend_from_slice(&(self.pps.len()as u16).to_be_bytes());avcc.extend_from_slice(&self.pps);
  let video=element(0xAE,&join(&[uint(0xD7,1),uint(0x73C5,1),uint(0x83,1),uint(0x9C,0),text(0x86,"V_MPEG4/ISO/AVC"),element(0x63A2,&avcc),uint(0x23E383,16_683_333),element(0xE0,&join(&[uint(0xB0,w as u64),uint(0xBA,h as u64)]))]));
  let mut opus=b"OpusHead".to_vec();opus.extend_from_slice(&[1,2]);opus.extend_from_slice(&312u16.to_le_bytes());opus.extend_from_slice(&48000u32.to_le_bytes());opus.extend_from_slice(&[0,0,0]);
  let audio=element(0xAE,&join(&[uint(0xD7,2),uint(0x73C5,2),uint(0x83,2),uint(0x9C,0),text(0x86,"A_OPUS"),element(0x63A2,&opus),uint(0x56AA,6_500_000),uint(0x56BB,80_000_000),element(0xE1,&join(&[element(0xB5,&48000f64.to_be_bytes()),uint(0x9F,2)]))]));
  self.pipe().write_all(&element(0x1654AE6B,&join(&[video,audio])))?;self.ready=true;crate::log!("MUX_READY width={w} height={h} video_copy=true");Ok(())
 }
 fn block(&mut self,track:u8,pts:u64,key:bool,data:&[u8])->Result<()>{
  if self.cluster.is_none_or(|t|pts.saturating_sub(t)>=1000||pts<t.saturating_sub(30000)){let base=pts/1000*1000;self.pipe().write_all(&id(0x1F43B675))?;self.pipe().write_all(&[1,255,255,255,255,255,255,255])?;self.pipe().write_all(&uint(0xE7,base))?;self.cluster=Some(base);}
  let rel=pts as i64-self.cluster.unwrap()as i64;if !(-32768..=32767).contains(&rel){bail!("Media interleave out of range");}
  let mut b=Vec::with_capacity(4+data.len());b.push(0x80|track);b.extend_from_slice(&(rel as i16).to_be_bytes());b.push(if key{0x80}else{0});b.extend_from_slice(data);self.pipe().write_all(&element(0xA3,&b))?;Ok(())
 }
 pub fn video(&mut self,sample:Sample<'_>)->Result<()>{
  let pts=self.video_clock.timestamp(sample.time,self.started.elapsed().as_micros()as u64);let units=nals(sample.data);let key=sample.is_keyframe||units.iter().any(|n|n[0]&31==5);
  for n in &units{match n[0]&31{7=>{if self.ready{self.check_sps_change(n)?;}self.sps=n.to_vec();},8=>self.pps=n.to_vec(),_=>{}}}
  match frame_decision(sample.is_contiguous,key,self.await_key){
   FrameDecision::Write=>{}
   FrameDecision::WaitForKey=>{if !self.await_key{crate::log!("VIDEO_FRAME_DROPPED key={key} reason={}",if sample.is_contiguous{"awaiting_key"}else{"incomplete"});}
    self.await_key=true;return Ok(());}
  }
  if self.sps.is_empty()||self.pps.is_empty(){return Ok(());}
  if !self.ready{self.header()?;while let Some((t,b))=self.queued_audio.pop_front(){self.block(2,t,true,&b)?;}}
  self.await_key=false;let mut frame=Vec::with_capacity(sample.data.len());for nal in units{frame.extend_from_slice(&(nal.len()as u32).to_be_bytes());frame.extend_from_slice(nal);}
  self.block(1,pts,key,&frame)?;self.video_frames+=1;Ok(())
 }
 pub fn audio(&mut self,sample:Sample<'_>)->Result<()>{
  let pts=self.audio_clock.timestamp(sample.time,self.started.elapsed().as_micros()as u64);if self.ready{self.block(2,pts,true,sample.data)?;}else{if self.queued_audio.len()>=200{self.queued_audio.pop_front();}self.queued_audio.push_back((pts,sample.data.to_vec()));}
  self.audio_packets+=1;Ok(())
 }
 pub fn tick(&mut self)->Result<()>{if let Some(status)=self.child.try_wait()?{bail!("FFmpeg relay exited: {status}");}Ok(())}
 // 正常收尾：关掉输入，让 ffmpeg 把手里的数据写完再退出。测试用它拿到完整的输出文件
 #[cfg(test)]
 fn finish(&mut self){if let Some(pipe)=self.pipe.as_mut(){let _=pipe.flush();}self.pipe=None;let _=self.child.wait();}
}
impl Drop for MediaRelay{fn drop(&mut self){if let Some(pipe)=self.pipe.as_mut(){let _=pipe.flush();}let _=self.child.kill();let _=self.child.wait();}}

#[cfg(test)]
mod tests {
 use super::*;
 // 测试用的 SPS 生成器：按 Baseline 档逐位写一份最简 SPS，可带底边裁切和 VUI
 struct Bits{bytes:Vec<u8>,free:u8}
 impl Bits{
  fn put(&mut self,value:u32,width:usize){
   for shift in (0..width).rev(){
    if self.free==0{self.bytes.push(0);self.free=8;}
    let last=self.bytes.len()-1;self.bytes[last]|=(((value>>shift)&1)as u8)<<(self.free-1);self.free-=1;
   }
  }
  fn ue(&mut self,value:u32){let zeros=(32-(value+1).leading_zeros()-1)as usize;self.put(0,zeros);self.put(value+1,zeros+1);}
  fn finish(mut self)->Vec<u8>{self.put(1,1);while self.free>0&&self.free<8{self.put(0,1);}let mut out=vec![0x67];out.extend(self.bytes);out}
 }
 fn sps(width_mbs:u32,height_units:u32,crop_bottom:u32,vui:bool)->Vec<u8>{
  let mut b=Bits{bytes:vec![],free:0};
  b.put(66,8);b.put(0,8);b.put(30,8);b.ue(0);b.ue(0);b.ue(0);b.ue(0);b.ue(1);b.put(0,1);
  b.ue(width_mbs-1);b.ue(height_units-1);b.put(1,1);b.put(1,1);
  if crop_bottom>0{b.put(1,1);b.ue(0);b.ue(0);b.ue(0);b.ue(crop_bottom);}else{b.put(0,1);}
  if vui{b.put(1,1);b.put(1,1);b.put(1,32);b.put(60,32);b.put(1,1);b.put(0,1);b.put(0,1);b.put(0,1);b.put(0,1);b.put(0,1);b.put(0,1);}else{b.put(0,1);}
  b.finish()
 }
 #[test]
 fn parses_dimensions_from_sps(){
  assert_eq!(dimensions(&sps(160,90,0,false)).unwrap(),(2560,1440));
  assert_eq!(dimensions(&sps(120,68,4,false)).unwrap(),(1920,1080));
 }
 #[test]
 fn sps_sizes_report_both_sides(){
  let (current,incoming)=(sps(160,90,0,false),sps(120,68,4,true));
  assert_eq!(sps_sizes(&current,&incoming).unwrap(),((2560,1440),(1920,1080)));
 }
 #[test]
 fn broken_sps_is_reported_instead_of_accepted(){
  let mut short=sps(160,90,0,false);short.truncate(5);
  assert!(sps_sizes(&short,&sps(160,90,0,true)).is_err());
 }
 #[test]
 fn nals_are_split_at_three_and_four_byte_start_codes(){
  let stream=[0,0,0,1,0x67,1,2,0,0,1,0x68,3,0,0,0,1,0x65,4,5,6];
  assert_eq!(nals(&stream),vec![&stream[4..7] as &[u8],&stream[10..12],&stream[16..20]]);
 }
 #[test]
 fn clock_follows_source_and_reanchors_on_rewind(){
  let mut clock=Clock::default();
  assert_eq!(clock.timestamp(1_000_000,5_000_000),5000);
  assert_eq!(clock.timestamp(2_000_000,0),6000);
  // 网络乱序造成的几毫秒倒退：照常换算，不重新对齐
  assert_eq!(clock.timestamp(1_999_000,0),5999);
  // 主机重新配置编码器，源时间戳从头开始：接在上一帧之后，不回到过去
  assert_eq!(clock.timestamp(500_000,0),6000);
  assert_eq!(clock.timestamp(1_500_000,0),7000);
 }
 // 下面是端到端验证：把两段视频先后喂给真实的 ffmpeg，输出文件必须能完整解码。
 // 测试用的音频包：libopus 编出来的一个 20 毫秒双声道 440 赫兹音频帧（160 字节）。
 // 不用静音包，因为纯静音只有 3 个字节，ffmpeg 8 的解析器会把它判成非法
 const TONE_PACKET_HEX:&str=concat!(
  "bcb03243fc2d6222b45eee8e740bbc89002001fd4567dc6deedf7d3ab91ac3f2",
  "a9204f35c4cddb264f931fd04f0200a4ca84fa150ae802485d9758b8293e20a7",
  "0a71ca1fe8268e7c8614440db79d220e687f56d9dfd1a9016064b7140b778dfc",
  "6995b7a957d61c5baf0c98ab9ff4ff26972777b456f615ebcbfae837b52771d3",
  "728701ebce6c692b2a2cce9459fd2a63ff007616da0bea122ab19dc5291631ae",
 );
 fn hex_bytes(hex:&str)->Vec<u8>{
  (0..hex.len()/2).map(|i|u8::from_str_radix(&hex[i*2..i*2+2],16).expect("十六进制字节")).collect()
 }
 // 需要系统里有 ffmpeg（含 x264），默认跳过，在装了 ffmpeg 的容器里加 --ignored 运行
 static ENV_LOCK:std::sync::Mutex<()> = std::sync::Mutex::new(());
 const SWAP:usize=300;
 // 一共喂进去 900 个音频包，转成 AAC 后实测 ffmpeg 6.1、7.1、8.0 都是 1405 帧。低于 1000 帧说明音频丢了
 const AUDIO_MIN_FRAMES:usize=1000;
 // 用 x264 编一段每帧都带 AUD 的测试视频，加了噪点让码率不至于太低。
 // 带色彩描述时 SPS 里会多出 VUI，得到尺寸相同但内容不同的 SPS
 fn encode(size:&str,color:bool)->Vec<u8>{
  let mut cmd=Command::new("/usr/bin/ffmpeg");
  cmd.args(["-v","error","-f","lavfi","-i",&format!("testsrc=size={size}:rate=30"),"-frames:v","600","-vf","noise=alls=12:allf=t","-pix_fmt","yuv420p","-c:v","libx264","-profile:v","baseline","-x264-params","aud=1:keyint=30:scenecut=0:bframes=0","-f","h264"]);
  if color{cmd.args(["-color_primaries","bt709","-color_trc","bt709","-colorspace","bt709"]);}
  cmd.arg("-").stdout(Stdio::piped()).stderr(Stdio::null());
  let out=cmd.output().expect("运行 ffmpeg");assert!(out.status.success(),"ffmpeg 编码测试视频失败");out.stdout
 }
 // 按 AUD 把 AnnexB 码流切成帧
 fn frames(stream:&[u8])->Vec<Vec<u8>>{
  let mut frames:Vec<Vec<u8>>=vec![];
  for nal in nals(stream){
   if nal[0]&31==9||frames.is_empty(){frames.push(vec![]);}
   let last=frames.last_mut().unwrap();last.extend_from_slice(&[0,0,0,1]);last.extend_from_slice(nal);
  }
  frames
 }
 fn first_sps(frame:&[u8])->&[u8]{nals(frame).into_iter().find(|n|n[0]&31==7).expect("帧里应有 SPS")}
 fn has_idr(frame:&[u8])->bool{nals(frame).iter().any(|n|n[0]&31==5)}
 // 数一数输出文件里某一路流实际能解码出多少帧
 fn count_decoded(out:&std::path::Path,stream:&str)->usize{
  let probe=Command::new("/usr/bin/ffprobe").args(["-v","error","-count_frames","-select_streams",stream,"-show_entries","stream=nb_read_frames","-of","csv=p=0"]).arg(out).output().expect("运行 ffprobe");
  String::from_utf8_lossy(&probe.stdout).trim().trim_matches('"').parse().unwrap_or(0)
 }
 // 跑一遍完整的分发链路，再数输出里能解码出多少帧。日志文本只带在结果里帮助排查，不拿它判断成败：
 // ffmpeg 8 在流结束、清空 Opus 解析器时会打一条多余的 Error parsing Opus packet header，实际音频是完整的
 fn run_pipeline(tag:&str,before:&[Vec<u8>],after:&[Vec<u8>])->Decoded{
  let _env=ENV_LOCK.lock().unwrap();
  let dir=std::env::temp_dir().join(format!("xsx-relay-{tag}-{}",std::process::id()));
  std::fs::create_dir_all(&dir).unwrap();
  unsafe{std::env::set_var("STATE_DIR",&dir);}
  let out=dir.join("out.flv");
  let mut relay=MediaRelay::new(out.to_str().unwrap()).unwrap();
  let tone=hex_bytes(TONE_PACKET_HEX);
  let mut time=0u64;
  for (index,frame) in before.iter().take(SWAP).chain(after.iter()).enumerate(){
   // 切换编码参数时源时间戳从头开始，模拟主机重新配置编码器
   if index==SWAP{time=0;}
   relay.audio(Sample{data:&tone,time,is_contiguous:true,is_keyframe:false}).unwrap();
   relay.video(Sample{data:frame,time,is_contiguous:true,is_keyframe:has_idr(frame)}).unwrap();
   relay.tick().expect("ffmpeg 不应退出");
   time+=33_333;
  }
  let written=relay.video_frames as usize;
  relay.finish();
  let video=count_decoded(&out,"v:0");
  let audio=count_decoded(&out,"a:0");
  let logs=std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap_or_default();
  let _=std::fs::remove_dir_all(&dir);
  Decoded{video,audio,report:format!("{written} 帧写入；{logs}")}
 }
 struct Decoded{video:usize,audio:usize,report:String}
 #[test] #[ignore="需要系统里有 ffmpeg"]
 fn ffmpeg_survives_sps_change_with_same_size(){
  let (before,after)=(frames(&encode("320x180",false)),frames(&encode("320x180",true)));
  let (sps_before,sps_after)=(first_sps(&before[0]).to_vec(),first_sps(&after[0]).to_vec());
  assert_eq!(dimensions(&sps_before).unwrap(),dimensions(&sps_after).unwrap(),"两段视频的尺寸应当相同");
  assert_ne!(sps_before,sps_after,"两段视频的 SPS 应当不同");
  let decoded=run_pipeline("same-size",&before,&after);
  assert_eq!(decoded.video,SWAP+after.len(),"输出里解码出的视频帧数不对：{}",decoded.report);
  assert!(decoded.audio>=AUDIO_MIN_FRAMES,"输出里解码出的音频帧数太少：{}",decoded.report);
 }
 #[test] #[ignore="需要系统里有 ffmpeg"]
 fn ffmpeg_survives_resolution_change(){
  let (before,after)=(frames(&encode("320x180",false)),frames(&encode("256x144",false)));
  assert_eq!(sps_sizes(first_sps(&before[0]),first_sps(&after[0])).unwrap(),((320,180),(256,144)));
  let decoded=run_pipeline("new-size",&before,&after);
  assert_eq!(decoded.video,SWAP+after.len(),"输出里解码出的视频帧数不对：{}",decoded.report);
  assert!(decoded.audio>=AUDIO_MIN_FRAMES,"输出里解码出的音频帧数太少：{}",decoded.report);
 }
 // 帧不完整（丢了 UDP 包）时一律不写，关键帧也不例外：残缺的关键帧会让后面一整组帧解码报错。
 // 丢过帧后要等下一个完整的关键帧；完整的普通帧在正常时照常写
 #[test]
 fn incomplete_frames_are_never_written(){
  use FrameDecision::*;
  assert_eq!(frame_decision(false,true,false),WaitForKey,"残缺的关键帧不能写出去");
  assert_eq!(frame_decision(false,false,false),WaitForKey,"残缺的普通帧不能写出去");
  assert_eq!(frame_decision(true,false,true),WaitForKey,"丢过帧后，普通帧要等关键帧");
  assert_eq!(frame_decision(false,true,true),WaitForKey,"等待期间又来一个残缺的关键帧，继续等");
  assert_eq!(frame_decision(true,true,true),Write,"完整的关键帧到了才恢复");
  assert_eq!(frame_decision(true,false,false),Write,"正常时完整的关键帧和普通帧都写");
 }
}
