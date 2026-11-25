use crate::error::{Result,XboxError};
use chrono::{DateTime,Utc};
use serde_json::{Value,json};
use std::{path::PathBuf,os::unix::fs::OpenOptionsExt,io::Write};
#[derive(Clone)]
pub struct XboxAuth { path:PathBuf }
// 凭据文件的互斥锁。续期和「管理页登录成功后整体替换凭据」都要拿它，
// 避免新的凭据被一次尚未结束的续期覆盖回去
static SESSION_LOCK:tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
impl XboxAuth {
 pub fn new()->Self{Self{path:std::env::var_os("XBOX_AUTH_FILE").map(PathBuf::from).unwrap_or_else(||PathBuf::from("/auth/session.json"))}}
 fn read(&self)->Result<Value>{Ok(serde_json::from_slice(&std::fs::read(&self.path)?)?)}
 fn save(&self,d:&Value)->Result<()>{
  let tmp=self.path.with_extension(format!("tmp-{}",std::process::id()));
  let mut f=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
  let result=(||->Result<()>{f.write_all(&serde_json::to_vec(d)?)?;f.sync_all()?;std::fs::rename(&tmp,&self.path)?;Ok(())})();
  if result.is_err(){let _=std::fs::remove_file(&tmp);}result
 }
 pub async fn load_cached_tokens(&self)->Result<bool>{
  let d=self.read()?;Ok(d["refresh_token"].is_string()&&d["client_id"].is_string())
 }
 async fn json_response(r:reqwest::Response)->Result<Value>{
  let status=r.status();let body=r.json::<Value>().await?;
  if !status.is_success(){return Err(XboxError::AuthError(format!("HTTP {} code={}",status,body.get("error").or_else(||body.get("XErr")).map(|x|x.to_string()).unwrap_or_else(||"unknown".into()))));}Ok(body)
 }
 async fn refresh(&self,d:&mut Value)->Result<()>{
  let client=reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
  let id=d["client_id"].as_str().ok_or_else(||XboxError::AuthError("Missing OAuth client ID".into()))?.to_owned();
  let refresh=d["refresh_token"].as_str().ok_or_else(||XboxError::AuthError("Missing refresh authorization".into()))?.to_owned();
  let scope=d["scope"].as_str().unwrap_or("xboxlive.signin openid profile offline_access").to_owned();
  let r=client.post("https://login.microsoftonline.com/consumers/oauth2/v2.0/token").form(&[("client_id",id.as_str()),("grant_type","refresh_token"),("refresh_token",refresh.as_str()),("scope",scope.as_str())]).send().await?;
  let m=Self::json_response(r).await?;
  if let Some(v)=m.get("refresh_token"){d["refresh_token"]=v.clone();self.save(d)?;}
  let access=m["access_token"].as_str().ok_or_else(||XboxError::AuthError("OAuth access response missing".into()))?;
  let body=json!({"Properties":{"AuthMethod":"RPS","SiteName":"user.auth.xboxlive.com","RpsTicket":format!("d={access}")},"RelyingParty":"http://auth.xboxlive.com","TokenType":"JWT"});
  let u=Self::json_response(client.post("https://user.auth.xboxlive.com/user/authenticate").header("x-xbl-contract-version","1").json(&body).send().await?).await?;
  let token=u["Token"].as_str().ok_or_else(||XboxError::AuthError("Xbox user response missing".into()))?;
  let body=json!({"Properties":{"SandboxId":"RETAIL","UserTokens":[token]},"RelyingParty":"http://gssv.xboxlive.com/","TokenType":"JWT"});
  let x=Self::json_response(client.post("https://xsts.auth.xboxlive.com/xsts/authorize").header("x-xbl-contract-version","1").json(&body).send().await?).await?;
  if !x["Token"].is_string()||!x["NotAfter"].is_string()||!x["DisplayClaims"]["xui"][0]["uhs"].is_string(){return Err(XboxError::AuthError("Xbox streaming response incomplete".into()));}
  d["xsts_token"]=x["Token"].clone();d["expires_at"]=x["NotAfter"].clone();d["user_hash"]=x["DisplayClaims"]["xui"][0]["uhs"].clone();self.save(d)?;
  crate::log!("AUTH refreshed=true");Ok(())
 }
 pub async fn get_auth_header(&self)->Result<String>{
  let _session=SESSION_LOCK.lock().await;let mut d=self.read()?;
  let remaining=d["expires_at"].as_str().and_then(|s|DateTime::parse_from_rfc3339(s).ok()).map(|t|t.timestamp()-Utc::now().timestamp()).unwrap_or(0);
  if remaining<300||!d["xsts_token"].is_string(){self.refresh(&mut d).await?;}
  d["xsts_token"].as_str().map(str::to_owned).ok_or_else(||XboxError::AuthError("Streaming authorization unavailable".into()))
 }
 // 用登录流程拿到的新凭据整体替换凭据文件，旧的续期信息一并丢弃
 pub async fn store_session(&self,client_id:&str,refresh_token:&str,scope:&str)->Result<()>{
  let _session=SESSION_LOCK.lock().await;
  self.save(&json!({"client_id":client_id,"refresh_token":refresh_token,"scope":scope}))
 }
}
