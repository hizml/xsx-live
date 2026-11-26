"use strict";
const byId=id=>document.getElementById(id);
let data=null,csrf="",busy=false,previous=null,measuredFps=null,preview=null,toastTimer=null;
const actionButtons=[...document.querySelectorAll("[data-action]")];

// 语言：网址里的 ?lang= 最优先（并且记下来），其次是上次在这个浏览器里选的，最后看浏览器自己的语言
const LANG_KEY="xsx-lang";
function isLanguage(value){return value==="zh"||value==="en";}
function readStoredLanguage(){try{return localStorage.getItem(LANG_KEY);}catch(e){return null;}}
function saveLanguage(value){try{localStorage.setItem(LANG_KEY,value);}catch(e){}}
function pickLanguage(){
 const wanted=new URLSearchParams(location.search).get("lang");
 if(isLanguage(wanted)){saveLanguage(wanted);return wanted;}
 const stored=readStoredLanguage();
 if(isLanguage(stored))return stored;
 return(navigator.language||"").toLowerCase().startsWith("zh")?"zh":"en";
}
let lang=pickLanguage();
// 取当前语言的文字。两种语言都没有这个键时原样返回键名，页面上一眼能看出漏了哪一条。
// 键都写全（不拼字符串），接收端的测试会把这里用到的键和 i18n.js 逐个对上
function t(key,params){
 let text=I18N[lang][key]??I18N.zh[key]??key;
 if(params)for(const[name,value]of Object.entries(params))text=text.split(`{${name}}`).join(value);
 return text;
}
// 接口的报错按代码翻译，补充信息（例如微软返回的错误码）放在括号里；没有代码的旧接口给什么显示什么
function describe(code,detail,fallback){
 const text=I18N[lang][code]??I18N.zh[code];
 if(!text)return fallback||t("request_failed");
 return detail?text+(lang==="zh"?"（"+detail+"）":" ("+detail+")"):text;
}
// 请求没发出去或者超时的时候，浏览器给的报错是英文的技术文字，换成当前语言的说明；其余的报错原样显示
function failureText(error,fallback){return error.name==="TypeError"||error.name==="AbortError"?t("request_failed"):error.message||fallback;}
// 从管理页打开播放页时带上当前的语言，两个页面的语言保持一致
function withLanguage(url){const target=new URL(url);target.searchParams.set("lang",lang);return target.toString();}
// 把页面上带 data-i18n 的文字换成当前语言，再重画一遍动态内容
function applyLanguage(){
 document.documentElement.lang=lang==="zh"?"zh-CN":"en";
 document.title=t("title");
 for(const node of document.querySelectorAll("[data-i18n]"))node.textContent=t(node.dataset.i18n);
 for(const node of document.querySelectorAll("[data-i18n-aria-label]"))node.setAttribute("aria-label",t(node.dataset.i18nAriaLabel));
 for(const node of document.querySelectorAll("[data-i18n-placeholder]"))node.placeholder=t(node.dataset.i18nPlaceholder);
 for(const button of document.querySelectorAll("[data-lang]"))button.setAttribute("aria-pressed",String(button.dataset.lang===lang));
 byId("previewToggle").textContent=preview?t("preview_close"):t("preview_open");
 if(preview)preview.title=t("preview_frame_title");
 if(data){render(data);return;}
 byId("lastSync").textContent=t("sync_connecting");
 byId("accountState").textContent=t("account_checking");
 byId("accountTag").textContent=t("account_tag_checking");
}
function toast(message){byId("toast").textContent=message;byId("toast").hidden=false;clearTimeout(toastTimer);toastTimer=setTimeout(()=>byId("toast").hidden=true,2800);}
function duration(seconds){seconds=Math.max(0,Math.floor(seconds||0));const h=Math.floor(seconds/3600),m=Math.floor(seconds%3600/60),s=seconds%60;return h?[h,m,s].map(x=>String(x).padStart(2,"0")).join(":"):[m,s].map(x=>String(x).padStart(2,"0")).join(":");}
function buttons(){const enabled=Boolean(data?.control?.enabled);byId("start").disabled=busy||!csrf||enabled;byId("stop").disabled=busy||!csrf||!enabled;byId("restart").disabled=busy||!csrf;byId("autoStart").disabled=busy||!csrf;}
// 接收端的各种情况对应管理页的标题和说明（i18n.js 里的键）；tone 是状态灯的颜色，不写就是 pending
const STATE_TEXTS={
 paused:{title:"state_paused_title",detail:"state_paused_detail",tone:"idle"},
 stale:{title:"state_stale_title",detail:"state_stale_detail",tone:"error"},
 live:{title:"state_live_title",detail:"state_live_detail",tone:"live"},
 retrying:{title:"state_retrying_title",detail:"state_retrying_detail"},
 connecting:{title:"state_connecting_title",detail:"state_connecting_detail"},
 preparing:{title:"state_preparing_title",detail:"state_preparing_detail"}
};
// 「等待主机开机」时管理页怎么写：先看状态文件里记的原因，没有特别原因就用默认文案
const WAITING_TEXTS={
 CONSOLE_SHUTTING_DOWN:{title:"state_shutting_down_title",detail:"state_shutting_down_detail"},
 default:{title:"state_waiting_title",detail:"state_waiting_detail"}
};
// 这几种状态下接收端本来就报得慢，超过这个秒数没更新才算过期；其余状态是 35 秒
const STALE_AFTER={connecting:180,waiting_console:90};
const ACTION_TOASTS={start:"toast_started",stop:"toast_stopping",restart:"toast_reconnecting"};
// 状态栏的标题和说明：按接收端的状态和开关的修订号挑出一组文字（i18n.js 里的键）。
// 开关刚动过、接收端还没跟上时显示「正在切换」；其余情况按状态先后判断
function pickStateText(receiver,control,details,media,age){
 if(details.control_revision!==control.revision)return {title:control.enabled?"state_starting_title":"state_stopping_title",detail:"state_switching_detail"};
 if(receiver.state==="paused")return STATE_TEXTS.paused;
 if(age>(STALE_AFTER[receiver.state]||35))return STATE_TEXTS.stale;
 if(receiver.state==="streaming"&&media.active)return STATE_TEXTS.live;
 if(receiver.state==="waiting_console")return WAITING_TEXTS[details.reason]||WAITING_TEXTS.default;
 if(receiver.state==="retrying")return STATE_TEXTS.retrying;
 if(receiver.state==="connecting")return STATE_TEXTS.connecting;
 return STATE_TEXTS.preparing;
}
// 分辨率、帧率、码率、时长。帧率是用前后两次状态之间的帧数差算出来的，接收端自己不报
function renderStats(isStreaming,details,media,control){
 const video=media.video||{};
 byId("resolution").textContent=isStreaming&&video.width?video.width+" × "+video.height:"—";
 if(!isStreaming){previous=null;measuredFps=null;for(const id of ["fps","bitrate","uptime"])byId(id).textContent="—";return;}
 if(previous&&previous.revision===control.revision&&details.elapsed_seconds>previous.elapsed){measuredFps=(details.video_frames-previous.frames)/(details.elapsed_seconds-previous.elapsed);}
 if(!previous||previous.revision!==control.revision){measuredFps=null;}
 if(!previous||details.elapsed_seconds!==previous.elapsed){previous={revision:control.revision,elapsed:details.elapsed_seconds,frames:details.video_frames};}
 byId("fps").textContent=measuredFps!==null?measuredFps.toFixed(1)+" fps":t("stat_calculating");
 byId("bitrate").textContent=media.kbps>0?(media.kbps/1000).toFixed(1)+" Mb/s":t("stat_calculating");
 byId("uptime").textContent=duration(details.elapsed_seconds);
}
function renderLinks(value){
 for(const key of ["browser","flv","rtmp","dashboard"]){const input=byId("link-"+key);if(value.links?.[key])input.value=value.links[key];}
 if(value.links?.browser)byId("openBrowser").href=withLanguage(value.links.browser);
}
function render(value){
 data=value;csrf=value.csrf_token||"";
 const receiver=value.receiver||{},control=value.control||{},details=receiver.details||{},media=value.media||{};
 const age=Math.max(0,(value.now||Date.now()/1000)-(receiver.updated_at||0));
 const stateTexts=pickStateText(receiver,control,details,media,age);
 byId("stateTitle").textContent=t(stateTexts.title);byId("stateBadge").className="badge "+(stateTexts.tone||"pending");byId("stateDetail").textContent=t(stateTexts.detail);
 byId("lastSync").textContent=t("last_sync",{time:new Date().toLocaleTimeString(lang==="zh"?"zh-CN":"en-GB",{hour12:false})});
 // 接收端太旧时状态接口里没有版本号，如实提示，免得以为已经换了新版本
 byId("version").textContent=value.version?"v"+value.version:t("version_missing");
 const isStreaming=receiver.state==="streaming"&&control.enabled&&details.control_revision===control.revision&&age<35;
 renderStats(isStreaming,details,media,control);
 renderLinks(value);
 byId("autoStart").checked=Boolean(control.auto_start);
 renderAccount(value);
 buttons();
}
async function request(path,options={}){
 const abort=new AbortController(),timer=setTimeout(()=>abort.abort(),options.timeoutMs||6000),{timeoutMs,...init}=options;
 try{
  const response=await fetch(path,{cache:"no-store",credentials:"same-origin",...init,signal:abort.signal});
  const body=await response.json().catch(()=>null);
  if(!response.ok)throw new Error(describe(body?.code,body?.detail,body?.error));
  if(!body)throw new Error(t("request_failed"));
  return body;
 }finally{clearTimeout(timer);}
}
async function refresh(){try{render(await request("/api/status"));}catch(e){csrf="";buttons();byId("stateTitle").textContent=t("offline_title");byId("stateBadge").className="badge error";byId("stateDetail").textContent=t("offline_detail");}}
async function poll(){await refresh();setTimeout(poll,2000);}
for(const button of actionButtons)button.addEventListener("click",async()=>{
 if(busy||!csrf)return;busy=true;buttons();
 try{
  const action=button.dataset.action;
  await request("/api/control",{method:"POST",headers:{"Content-Type":"application/json","X-XSX-Token":csrf},body:JSON.stringify({action})});
  toast(t(ACTION_TOASTS[action]));
  await refresh();
 }catch(e){toast(failureText(e,t("action_failed")));}
 finally{busy=false;buttons();}
});
for(const button of document.querySelectorAll("[data-copy]"))button.addEventListener("click",async()=>{
 const input=byId("link-"+button.dataset.copy);if(!input.value){toast(t("link_missing"));return;}
 let copied=false;
 if(window.isSecureContext&&navigator.clipboard){try{await navigator.clipboard.writeText(input.value);copied=true;}catch(e){}}
 if(!copied){input.focus();input.select();input.setSelectionRange(0,input.value.length);try{copied=document.execCommand("copy");}catch(e){}}
 if(copied){button.textContent=t("copied");toast(t("toast_copied"));setTimeout(()=>button.textContent=t("button_copy"),1800);}else{toast(t("copy_manual"));}
});
// 「开机自动串流」开关。只改设置，不会打断正在进行的串流
byId("autoStart").addEventListener("change",async()=>{
 if(busy||!csrf)return;
 const enabled=byId("autoStart").checked;busy=true;buttons();
 try{
  await request("/api/control",{method:"POST",headers:{"Content-Type":"application/json","X-XSX-Token":csrf},body:JSON.stringify({action:"auto_start",enabled})});
  toast(t(enabled?"toast_auto_start_on":"toast_auto_start_off"));
 }catch(e){toast(failureText(e,t("setting_failed")));}
 finally{busy=false;await refresh();}
});
byId("previewToggle").addEventListener("click",()=>{
 if(preview){preview.remove();preview=null;byId("previewPlaceholder").hidden=false;byId("previewToggle").textContent=t("preview_open");return;}
 if(!data?.links?.preview){toast(t("preview_missing"));return;}
 preview=document.createElement("iframe");preview.title=t("preview_frame_title");preview.allow="autoplay; fullscreen";preview.src=withLanguage(data.links.preview);byId("previewPlaceholder").hidden=true;byId("previewArea").append(preview);byId("previewToggle").textContent=t("preview_close");
});
let loginUrl="",loginBusy=false,loginWasPending=false;
const LOGIN_HINTS={loopback:"login_hint_loopback",callback:"login_hint_callback"};
function renderAccount(value){
 const auth=value.auth;
 if(!auth){byId("accountState").textContent=t("account_unsupported");byId("accountTag").textContent=t("account_tag_unsupported");byId("loginSteps").hidden=true;return;}
 const canLogin=Boolean(auth.can_login),pending=Boolean(auth.busy);
 // 最近一次登录失败的原因。接收端较新时给的是代码加补充信息，按当前语言显示；较旧时只有一段中文
 const failed=auth.last_error_code?describe(auth.last_error_code,auth.last_error_detail,auth.last_error):auth.last_error||"";
 byId("accountState").textContent=auth.configured?(canLogin?t("account_expired"):t("account_valid")):t("account_none");
 byId("accountTag").textContent=auth.configured?(canLogin?t("account_tag_expired"):t("account_tag_signed_in")):t("account_tag_signed_out");
 byId("loginSteps").hidden=!canLogin;
 byId("loginHint").textContent=t(LOGIN_HINTS[auth.loopback?"loopback":"callback"]);
 byId("loginPasteRow").hidden=!canLogin||!loginUrl||!auth.loopback;
 byId("loginError").hidden=!failed;byId("loginError").textContent=failed?t("login_failed_prefix")+failed:"";
 byId("loginStart").disabled=busy||!csrf||pending||loginBusy;
 byId("loginFinish").disabled=busy||!csrf||pending||loginBusy||!byId("loginPaste").value.trim();
 byId("loginOpen").hidden=!canLogin||!loginUrl;if(loginUrl)byId("loginOpen").href=loginUrl;
 // 后台换取凭据结束时提示一次结果
 if(pending)loginWasPending=true;
 else if(loginWasPending){loginWasPending=false;loginUrl="";toast(failed?t("toast_login_failed"):t("toast_login_ok"));}
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
 }catch(e){toast(failureText(e,t("login_start_failed")));}
 finally{loginBusy=false;await refresh();}
});
byId("loginFinish").addEventListener("click",async()=>{
 const url=byId("loginPaste").value.trim();if(!url||busy||!csrf||loginBusy)return;loginBusy=true;renderAccount(data||{});
 // 网址里带着一次性的授权码，取出来后就从输入框里清掉，不在页面上留底
 byId("loginPaste").value="";
 try{
  const result=await submitLogin("/api/auth/callback",{url});
  if(result.pending)toast(t("login_pending"));
 }catch(e){toast(failureText(e,t("toast_login_failed")));}
 finally{loginBusy=false;await refresh();}
});
byId("loginPaste").addEventListener("input",()=>renderAccount(data||{}));
for(const button of document.querySelectorAll("[data-lang]"))button.addEventListener("click",()=>{lang=button.dataset.lang;saveLanguage(lang);applyLanguage();});
applyLanguage();
poll();
