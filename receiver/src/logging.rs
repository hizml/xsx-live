// 日志文件：接收端的每一行日志在写标准输出的同时，追加到状态目录里的 receiver.log。
// 这样从共享文件夹就能直接看日志，不必进 Container Station。
// 文件打不开就只写标准输出；写失败一律忽略，不让日志拖垮接收
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::{Path, PathBuf}, sync::Mutex};

const LOG_FILE: &str = "receiver.log";
const PREVIOUS_LOG_FILE: &str = "receiver.previous.log";
// 日志文件超过这个大小就转存为 receiver.previous.log，只保留一份旧的
const ROTATE_BYTES: u64 = 20_000_000;
// 每写这么多行检查一次文件大小
const ROTATE_CHECK_LINES: u32 = 4096;
// 文件模式：属主可写，其他人只读，共享文件夹里才看得到
const LOG_FILE_MODE: u32 = 0o644;

struct Sink { state_dir: PathBuf, file: std::fs::File, lines: u32 }
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

// 锁被污染说明之前有地方出过 panic，日志这边照常工作即可
fn lock() -> std::sync::MutexGuard<'static, Option<Sink>> {
    SINK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_current(state_dir: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).mode(LOG_FILE_MODE).open(state_dir.join(LOG_FILE))
}

// 日志文件超过 ROTATE_BYTES 时把它改名为 receiver.previous.log，返回有没有转存
fn rotate_if_oversized(state_dir: &Path) -> bool {
    let oversized = std::fs::metadata(state_dir.join(LOG_FILE)).map(|m| m.len() > ROTATE_BYTES).unwrap_or(false);
    oversized && std::fs::rename(state_dir.join(LOG_FILE), state_dir.join(PREVIOUS_LOG_FILE)).is_ok()
}

// 在状态目录里打开日志文件，目录不存在就建。已有的日志过大时先转存，再从新文件开始写
pub fn open(state_dir: &Path) {
    let _ = std::fs::create_dir_all(state_dir);
    let _ = rotate_if_oversized(state_dir);
    *lock() = open_current(state_dir).ok().map(|file| Sink { state_dir: state_dir.to_path_buf(), file, lines: 0 });
}

// 追加一行。每写 ROTATE_CHECK_LINES 行检查一次大小，超过就转存并换新文件
pub fn append(line: &[u8]) {
    let mut sink = lock();
    let Some(current) = sink.as_mut() else { return };
    if current.file.write_all(line).is_err() { return; }
    current.lines += 1;
    if current.lines < ROTATE_CHECK_LINES { return; }
    current.lines = 0;
    if !rotate_if_oversized(&current.state_dir) { return; }
    if let Ok(file) = open_current(&current.state_dir) { current.file = file; }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // 这些用例共用同一个日志句柄，必须一个跑完再跑下一个
    static SERIAL: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xsx-log-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lines_are_appended_to_a_world_readable_file() {
        let _serial = SERIAL.lock().unwrap();
        let dir = temp_dir("append").join("not/there/yet");
        open(&dir);
        append(b"first\n");
        append(b"second\n");
        assert_eq!(std::fs::read_to_string(dir.join(LOG_FILE)).unwrap(), "first\nsecond\n");
        let mode = std::fs::metadata(dir.join(LOG_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, LOG_FILE_MODE);
        std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn an_oversized_log_is_rotated_before_writing() {
        let _serial = SERIAL.lock().unwrap();
        let dir = temp_dir("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LOG_FILE), vec![b'x'; ROTATE_BYTES as usize + 1]).unwrap();
        open(&dir);
        append(b"fresh\n");
        assert!(dir.join(PREVIOUS_LOG_FILE).exists(), "旧日志应该被转存");
        assert_eq!(std::fs::read_to_string(dir.join(LOG_FILE)).unwrap(), "fresh\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_is_written_after_the_sink_is_closed() {
        let _serial = SERIAL.lock().unwrap();
        let dir = temp_dir("closed");
        *lock() = None;
        append(b"dropped\n");
        assert!(!dir.exists());
    }
}
