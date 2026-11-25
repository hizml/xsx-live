pub mod auth;
pub mod error;
pub mod xhome;
pub mod rtc { pub mod protocol; pub mod input; }
pub mod relay;
pub mod health;
pub mod control;
pub mod dashboard;
pub mod login;
pub mod assets;
pub mod logging;

// 程序版本号，来自 Cargo.toml，编进程序里。发布时打的 Git 标签就是 v 加上这个号
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// 带时间的日志：写到标准输出，同时追加到状态目录里的 receiver.log（启动时用 logging::open 打开过才会写文件）。
// 写失败一律忽略，不让日志拖垮接收
#[macro_export]
macro_rules! log {
 ($($arg:tt)*)=>{{
  use std::io::Write;
  let line=format!("[{}] {}\n",chrono::Local::now().format("%m-%d %H:%M:%S"),format_args!($($arg)*));
  let _=std::io::stdout().write_all(line.as_bytes());
  $crate::logging::append(line.as_bytes());
 }};
}
