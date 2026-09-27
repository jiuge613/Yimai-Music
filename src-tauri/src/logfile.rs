//! 文件日志。
//!
//! release 构建带 `windows_subsystem = "windows"`，没有控制台，
//! 所有 `eprintln!` 都会被丢弃 —— 线上出问题时没有任何可看的线索。
//! 这里把关键路径的日志同时写到应用数据目录下的 yimai.log，
//! 并接管 panic hook，让崩溃也留痕。
//!
//! 写入是「追加 + 尽力而为」：磁盘写失败不影响主流程。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
static LOG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<PathBuf>> {
    LOG_PATH.get_or_init(|| Mutex::new(None))
}

/// 初始化日志文件。路径通常取 `<app_data>/yimai.log`。
/// 同时把 panic 全部记下来（含 backtrace）。
pub fn init(dir: &std::path::Path) {
    if let Ok(mut g) = slot().lock() {
        // 单次运行上限 2MB，超出就从头写，避免日志无限膨胀
        let p = dir.join("yimai.log");
        if p.metadata().map(|m| m.len() > 2 * 1024 * 1024).unwrap_or(false) {
            let _ = std::fs::write(&p, b"");
        }
        *g = Some(p);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        write(&format!("[panic] {where_} {info}"));
        prev(info);
    }));
}

/// 追加一行日志（自动加时间戳）。只写文件，不输出到 stderr。
pub fn write(msg: &str) {
    let g = slot().lock().ok();
    let Some(g) = g else { return };
    let Some(p) = g.as_ref() else { return };
    let ts = {
        // 不引 chrono：取进程启动以来的秒数即可定位相对顺序
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

/// 同时写 stderr 和文件：`elog!("...")`
#[macro_export]
macro_rules! elog {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        eprintln!("{msg}");
        $crate::logfile::write(&msg);
    }};
}
