use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};

// 改开关时的互斥锁：管理页的请求和接收端自己在主机关机时的收尾都走这里
static CONTROL_LOCK:tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
// 旧版本的状态文件里没有 auto_start 这一项，按 true 读入，保持原来的行为
fn default_auto_start()->bool{true}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControlState {
    pub enabled: bool,
    pub revision: u64,
    pub updated_at: i64,
    /// 「开机自动串流」：主机关机后串流停止，下次开机要不要自动开始
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
}
impl Default for ControlState {
    fn default() -> Self {
        Self { enabled: true, revision: 0, updated_at: chrono::Utc::now().timestamp(), auto_start: true }
    }
}
// 指令名用 snake_case：Start、Stop、Restart 不变，AutoStart 是 auto_start
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action { Start, Stop, Restart, AutoStart }

/// 管理页发来的指令。只有 AutoStart 要带 enabled，表示把「开机自动串流」设成开还是关
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command { pub action: Action, pub enabled: Option<bool> }

impl Command {
    pub fn is_well_formed(&self)->bool{
        matches!((self.action,self.enabled),
            (Action::AutoStart,Some(_))|(Action::Start,None)|(Action::Stop,None)|(Action::Restart,None))
    }
}

impl ControlState {
    pub fn apply(&self, action: Action) -> Self {
        let enabled = !matches!(action, Action::Stop);
        if enabled == self.enabled && !matches!(action, Action::Restart) { return self.clone(); }
        Self { enabled, revision: self.revision.saturating_add(1), updated_at: chrono::Utc::now().timestamp(), auto_start: self.auto_start }
    }
    /// 只改「开机自动串流」。它不影响正在进行的串流，所以不动修订号
    pub fn with_auto_start(&self, auto_start: bool) -> Self {
        if auto_start == self.auto_start { return self.clone(); }
        Self { auto_start, ..self.clone() }
    }
    fn path() -> PathBuf {
        PathBuf::from(std::env::var("STATE_DIR").unwrap_or_else(|_| "/state".into())).join("control.json")
    }
    pub fn load() -> Result<Self> {
        let p = Self::path();
        match std::fs::read(&p) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let initial = Self::default(); initial.save()?; Ok(initial)
            }
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!("tmp-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        let result = (|| -> Result<()> {
            f.write_all(&serde_json::to_vec(self)?)?; f.sync_all()?;
            std::fs::rename(&tmp, &p)?; Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        result
    }
}

/// 应用一条指令：先保存，再广播给接收循环和管理页。返回状态是否真的变了
pub async fn apply_action(control: &tokio::sync::watch::Sender<ControlState>, command: &Command) -> Result<bool> {
    let _guard = CONTROL_LOCK.lock().await;
    let before = control.borrow().clone();
    let after = match command.action {
        Action::AutoStart => before.with_auto_start(command.enabled.unwrap_or(before.auto_start)),
        other => before.apply(other),
    };
    let changed = after != before;
    if changed {
        after.save()?;
        control.send_replace(after.clone());
        crate::log!("CONTROL enabled={} auto_start={} revision={}", after.enabled, after.auto_start, after.revision);
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_is_persistent_and_start_is_idempotent() {
        let original = ControlState::default();
        assert_eq!(original.apply(Action::Start), original);
        let stopped = original.apply(Action::Stop);
        assert!(!stopped.enabled); assert_eq!(stopped.revision, 1);
        assert_eq!(stopped.apply(Action::Stop), stopped);
        let stored: ControlState = serde_json::from_slice(&serde_json::to_vec(&stopped).unwrap()).unwrap();
        assert!(!stored.enabled);
        let running = stored.apply(Action::Start);
        assert!(running.enabled); assert_eq!(running.revision, 2);
    }
    #[test]
    fn restart_always_changes_revision() {
        let original = ControlState::default();
        let again = original.apply(Action::Restart);
        assert!(again.enabled); assert_eq!(again.revision, original.revision + 1);
    }
    #[test]
    fn rejects_arbitrary_actions_and_extra_fields() {
        assert!(serde_json::from_str::<Command>(r#"{"action":"shell"}"#).is_err());
        assert!(serde_json::from_str::<Command>(r#"{"action":"start","command":"anything"}"#).is_err());
    }
    #[test]
    fn auto_start_toggles_without_touching_the_session() {
        let original = ControlState::default();
        let off = original.with_auto_start(false);
        assert!(!off.auto_start); assert_eq!(off.revision, original.revision); assert!(off.enabled);
        assert_eq!(off.with_auto_start(false), off, "值没变时状态也不变");
        let stored: ControlState = serde_json::from_str(r#"{"enabled":false,"revision":3,"updated_at":1}"#).unwrap();
        assert!(stored.auto_start, "旧版本的状态文件没有这一项时按原来的行为处理");
    }
    #[test]
    fn auto_start_command_needs_a_value_and_others_need_none() {
        let parse = |body: &str| serde_json::from_str::<Command>(body).unwrap();
        assert!(parse(r#"{"action":"auto_start","enabled":false}"#).is_well_formed());
        assert!(!parse(r#"{"action":"auto_start"}"#).is_well_formed());
        assert!(!parse(r#"{"action":"start","enabled":true}"#).is_well_formed());
        assert!(parse(r#"{"action":"stop"}"#).is_well_formed());
    }
}
