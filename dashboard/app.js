"use strict";
const byId=id=>document.getElementById(id);
let data=null,csrf="",busy=false,previous=null,measuredFps=null,preview=null,toastTimer=null;
const actionButtons=[...document.querySelectorAll("[data-action]")];
function toast(message){byId("toast").textContent=message;byId("toast").hidden=false;clearTimeout(toastTimer);toastTimer=setTimeout(()=>byId("toast").hidden=true,2800);}
function duration(seconds){seconds=Math.max(0,Math.floor(seconds||0));const h=Math.floor(seconds/3600),m=Math.floor(seconds%3600/60),s=seconds%60;return h?[h,m,s].map(x=>String(x).padStart(2,"0")).join(":"):[m,s].map(x=>String(x).padStart(2,"0")).join(":");}
function buttons(){const enabled=Boolean(data?.control?.enabled);byId("start").disabled=busy||!csrf||enabled;byId("stop").disabled=busy||!csrf||!enabled;byId("restart").disabled=busy||!csrf;byId("autoStart").disabled=busy||!csrf;}
// 「等待主机开机」时管理页怎么写：先看状态文件里记的原因，没有特别原因就用默认文案
const WAITING_DEFAULT={title:"等待 XSX 开机",description:"串流开关开着，主机开机后会自动开始串流；接收器不会主动唤醒主机。"};
const WAITING_REASONS={
 CONSOLE_SHUTTING_DOWN:{title:"主机正在关机",description:"关机后主机要几分钟才彻底断电，这期间不再连接。等它关完会自动转为等待开机，下次开机自动开始串流。如果主机其实已经重新开机，点“重新连接 XSX”。"}
};
function render(value){
 data=value;csrf=value.csrf_token||"";
 const receiver=value.receiver||{},control=value.control||{},details=receiver.details||{},media=value.media||{};
 const age=Math.max(0,(value.now||Date.now()/1000)-(receiver.updated_at||0));
 let title,description,tone="pending";const revisionMatch=details.control_revision===control.revision;
 if(!revisionMatch){title=control.enabled?"正在开启 / 重新连接":"正在停止串流";description="正在完成会话切换，请稍候。";}
 else if(receiver.state==="paused"){title="串流已停止";description="自动重连已暂停。点击“开启串流”后重新接收画面。";tone="idle";}
 else if(age>(receiver.state==="connecting"?180:receiver.state==="waiting_console"?90:35)){title="接收状态未更新";description="管理页面可访问，但接收进程状态已过期。可尝试重新连接。";tone="error";}
 else if(receiver.state==="streaming"&&media.active){title="正在接收 · 正在分发";description="XSX 画面已进入局域网分发服务，可在直播伴侣拉流。";tone="live";}
 else if(receiver.state==="waiting_console"){const waiting=WAITING_REASONS[details.reason]||WAITING_DEFAULT;title=waiting.title;description=waiting.description;}
 else if(receiver.state==="retrying"){title="正在等待重连";description="当前连接已断开，接收器会自动重试。也可以点击“重新连接 XSX”。";}
 else if(receiver.state==="connecting"){title="正在连接 XSX";description="正在建立微软授权会话与局域网媒体连接。";}
 else {title="正在准备画面";description="接收器正在等待可分发的音视频。";}
 byId("stateTitle").textContent=title;byId("stateBadge").className="badge "+tone;byId("stateDetail").textContent=description;
 byId("lastSync").textContent="最后刷新 "+new Date().toLocaleTimeString("zh-CN",{hour12:false});
 // 接收端太旧时状态接口里没有版本号，如实提示，免得以为已经换了新版本
 byId("version").textContent=value.version?"v"+value.version:"版本过旧（不含版本号）";
 const streaming=receiver.state==="streaming"&&control.enabled&&revisionMatch&&age<35;
 const video=media.video||{};
 byId("resolution").textContent=streaming&&video.width?video.width+" × "+video.height:"—";
 if(streaming){
  if(previous&&previous.revision===control.revision&&details.elapsed_seconds>previous.elapsed){measuredFps=(details.video_frames-previous.frames)/(details.elapsed_seconds-previous.elapsed);}
  if(!previous||previous.revision!==control.revision){measuredFps=null;}
  if(!previous||details.elapsed_seconds!==previous.elapsed){previous={revision:control.revision,elapsed:details.elapsed_seconds,frames:details.video_frames};}
  byId("fps").textContent=measuredFps!==null?measuredFps.toFixed(1)+" fps":"统计中";
  byId("bitrate").textContent=media.kbps>0?(media.kbps/1000).toFixed(1)+" Mb/s":"统计中";
  byId("uptime").textContent=duration(details.elapsed_seconds);
 }else{previous=null;measuredFps=null;for(const id of ["fps","bitrate","uptime"])byId(id).textContent="—";}
 for(const key of ["browser","flv","rtmp","dashboard"]){const input=byId("link-"+key);if(value.links?.[key])input.value=value.links[key];}
 if(value.links?.browser)byId("openBrowser").href=value.links.browser;
 byId("autoStart").checked=Boolean(control.auto_start);
 renderAccount(value);
 buttons();
}
async function request(path,options={}){
 const abort=new AbortController(),timer=setTimeout(()=>abort.abort(),options.timeoutMs||6000),{timeoutMs,...init}=options;
 try{const response=await fetch(path,{cache:"no-store",credentials:"same-origin",...init,signal:abort.signal});const body=await response.json();if(!response.ok)throw new Error(body.error||"请求失败");return body;}finally{clearTimeout(timer);}
}
async function refresh(){try{render(await request("/api/status"));}catch(e){csrf="";buttons();byId("stateTitle").textContent="管理服务连接中";byId("stateBadge").className="badge error";byId("stateDetail").textContent="暂时无法读取状态，将自动重试。已有串流不受此页面断线影响。";}}
async function poll(){await refresh();setTimeout(poll,2000);}
for(const button of actionButtons)button.addEventListener("click",async()=>{
 if(busy||!csrf)return;busy=true;buttons();
 try{
  const action=button.dataset.action;
  await request("/api/control",{method:"POST",headers:{"Content-Type":"application/json","X-XSX-Token":csrf},body:JSON.stringify({action})});
  toast(action==="start"?"已开启接收，正在连接 XSX":action==="stop"?"正在停止接收，开关状态已保存":"正在重新连接 XSX");
  await refresh();
 }catch(e){toast(e.message||"操作失败，请刷新后重试");}
 finally{busy=false;buttons();}
});
for(const button of document.querySelectorAll("[data-copy]"))button.addEventListener("click",async()=>{
 const input=byId("link-"+button.dataset.copy);if(!input.value){toast("地址尚未加载");return;}
 let copied=false;
 if(window.isSecureContext&&navigator.clipboard){try{await navigator.clipboard.writeText(input.value);copied=true;}catch(e){}}
 if(!copied){input.focus();input.select();input.setSelectionRange(0,input.value.length);try{copied=document.execCommand("copy");}catch(e){}}
 if(copied){button.textContent="已复制";toast("地址已复制");setTimeout(()=>button.textContent="复制",1800);}else{toast("已选中地址，请长按或按 Ctrl+C 复制");}
});
// 「开机自动串流」开关。只改设置，不会打断正在进行的串流
byId("autoStart").addEventListener("change",async()=>{
 if(busy||!csrf)return;
 const enabled=byId("autoStart").checked;busy=true;buttons();
 try{
  await request("/api/control",{method:"POST",headers:{"Content-Type":"application/json","X-XSX-Token":csrf},body:JSON.stringify({action:"auto_start",enabled})});
  toast(enabled?"主机关机后，下次开机会自动开始串流":"主机关机后不再自动开始串流");
 }catch(e){toast(e.message||"设置失败，请刷新后重试");}
 finally{busy=false;await refresh();}
});
byId("previewToggle").addEventListener("click",()=>{
 if(preview){preview.remove();preview=null;byId("previewPlaceholder").hidden=false;byId("previewToggle").textContent="打开预览";return;}
 if(!data?.links?.preview){toast("预览地址尚未加载");return;}
 preview=document.createElement("iframe");preview.title="XSX 静音画面预览";preview.allow="autoplay; fullscreen";preview.src=data.links.preview;byId("previewPlaceholder").hidden=true;byId("previewArea").append(preview);byId("previewToggle").textContent="关闭预览";
});
let loginUrl="",loginBusy=false,loginWasPending=false;
const LOGIN_HINTS={
 loopback:"点「开始登录」，用 Xbox 上的账号在微软页面登录。之后浏览器会显示“无法访问此网站”，这是正常的：把地址栏里的完整网址粘贴到下面，再点「完成登录」。",
 callback:"点「开始登录」，用 Xbox 上的账号在微软页面登录并同意授权，完成后会自动跳回这里。"
};
function renderAccount(value){
 const auth=value.auth;
 if(!auth){byId("accountState").textContent="当前接收端版本不支持在管理页登录。";byId("accountTag").textContent="不支持";byId("loginSteps").hidden=true;return;}
 const canLogin=Boolean(auth.can_login),pending=Boolean(auth.busy),failed=auth.last_error||"";
 byId("accountState").textContent=auth.configured?(canLogin?"已保存的登录已失效，需要重新登录。":"已登录，凭据有效。"):"尚未登录。登录后接收端才能连接 XSX。";
 byId("accountTag").textContent=auth.configured?(canLogin?"已失效":"已登录"):"未登录";
 byId("loginSteps").hidden=!canLogin;
 byId("loginHint").textContent=LOGIN_HINTS[auth.loopback?"loopback":"callback"];
 byId("loginPasteRow").hidden=!canLogin||!loginUrl||!auth.loopback;
 byId("loginError").hidden=!failed;byId("loginError").textContent=failed?"登录失败："+failed:"";
 byId("loginStart").disabled=busy||!csrf||pending||loginBusy;
 byId("loginFinish").disabled=busy||!csrf||pending||loginBusy||!byId("loginPaste").value.trim();
 byId("loginOpen").hidden=!canLogin||!loginUrl;if(loginUrl)byId("loginOpen").href=loginUrl;
 // 后台换取凭据结束时提示一次结果
 if(pending)loginWasPending=true;
 else if(loginWasPending){loginWasPending=false;loginUrl="";toast(failed?"登录失败，请重试":"登录成功，正在连接 XSX");}
}
async function submitLogin(path,body){
 return request(path,{method:"POST",timeoutMs:12000,headers:{"Content-Type":"application/json","X-XSX-Token":csrf},body:JSON.stringify(body)});
}
byId("loginStart").addEventListener("click",async()=>{
 if(busy||!csrf||loginBusy)return;loginBusy=true;renderAccount(data||{});
 try{
  const result=await submitLogin("/api/auth/start",{});
  loginUrl=result.url;byId("loginPaste").value="";
  window.open(loginUrl,"_blank","noopener");
 }catch(e){toast(e.message||"无法开始登录，请刷新后重试");}
 finally{loginBusy=false;await refresh();}
});
byId("loginFinish").addEventListener("click",async()=>{
 const url=byId("loginPaste").value.trim();if(!url||busy||!csrf||loginBusy)return;loginBusy=true;renderAccount(data||{});
 // 网址里带着一次性的授权码，取出来后就从输入框里清掉，不在页面上留底
 byId("loginPaste").value="";
 try{
  const result=await submitLogin("/api/auth/callback",{url});
  if(result.pending)toast("正在完成登录，请稍候");
 }catch(e){toast(e.message||"登录失败，请重试");}
 finally{loginBusy=false;await refresh();}
});
byId("loginPaste").addEventListener("input",()=>renderAccount(data||{}));
poll();
