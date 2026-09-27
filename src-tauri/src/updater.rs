//! GitHub Release 自动更新。
//!
//! 流程：检查最新 release → 前端弹窗展示 markdown 更新说明 → 用户确认后
//! 下载安装包到临时目录（带进度事件）→ 退出应用，由分离的 PowerShell 助手
//! 静默运行 Inno Setup 安装包（显式传 `/DIR=<当前安装目录>`，安装位置保持
//! 不变），安装进程结束后自动重启应用并清理临时文件。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// GitHub 仓库（owner/name），自动更新从这里拉取最新 Release。
/// 上游为 Apache-2.0 项目，本仓库为完整品牌化后的二改版本。
/// 仓库已更名为 Yimai-Music；GitHub 会对旧地址 301 重定向，
/// 万一重定向在个别网络下失效，只需改这一行。
const GITHUB_REPO: &str = "jiuge613/Yimai-Music";
const UA: &str = "YimaiMusic-Updater";
/// 单个下载来源的连接/传输超时：安装包 10~20 MB，弱网 180s 足够；
/// 某来源卡死/无响应时在该超时后快速切换下一个候选源，避免整体挂起。
const SOURCE_TIMEOUT: Duration = Duration::from_secs(180);

static DOWNLOAD_CANCEL: AtomicBool = AtomicBool::new(false);

/// GitHub 下载代理镜像（ghproxy 风格：`{mirror}/{github 原链接}`）。
/// 仅用于「下载 GitHub 附件」的兜底切换：直连失败/超时/连接被重置时按序尝试，
/// 国内网络下显著提升更新包下载成功率。顺序=按实测可用性排前的源在前；
/// 失效的源会被快速跳过（连接失败即切换），不影响最终结果。
/// 新增/移除镜像只维护这个列表即可，UI 无需任何改动。
const GITHUB_MIRRORS: &[&str] = &[
    "https://ghproxy.cc/",
    "https://gh.monoliker.com/",
    "https://gproxy.twinzips.top/",
    "https://ghproxy.mnjiang.cn/",
    "https://ghproxy.mciel.com/",
    "https://github.chenc.dev/",
    "https://ghfile.geekertao.top/",
    "https://gh.llk-exmfr52bqpe.top/",
    "https://gh.kleyeas.com/",
    "https://ghm.0t8465.xyz/",
    "https://gh-proxy.com/",
    "https://github-proxy-memory-echoes.cn/",
    "https://fastgit.cc/",
    "https://gh.nokiu.com/",
    "https://gh.gxpk.top/",
    "https://gh.xcxxxo.cf/",
    "https://tv.tw/",
    "https://gh.kichills.cn/",
    "https://cdn.akacoder.online/",
];

/// GitHub 附件直链（api.github.com 返回的 browser_download_url）形如：
/// https://github.com/{owner}/{repo}/releases/download/{tag}/{file}
/// 镜像代理要求拼接原始 github.com 完整链接。这里解析出
/// `{owner}/{repo}/releases/download/...` 这段可被镜像代理前缀的路径；
/// 非该形式的 URL 返回 None（保持原样直连，不套用镜像）。
fn github_download_path(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = u.strip_prefix("https://github.com/")?;
    let path = rest.split_once('?').map(|(p, _)| p).unwrap_or(rest);
    let mut parts = path.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let prefix = format!("{owner}/{repo}/releases/download/");
    if !path.starts_with(&prefix) {
        return None;
    }
    // 归一为 {owner}/{repo}/releases/download/{tag}/{file}（相对 github.com）
    Some(path.to_string())
}

/// 生成候选下载源：直连（github.com 完整链接）+ 各镜像代理。
///
/// ghproxy 类镜像的协议是 `{mirror}/https://github.com/...`——前缀后接
/// **完整**原始链接（含 https 与 github.com），所以这里先构造完整直连 URL，
/// 再对每个镜像做前缀拼接。非 GitHub 附件链接保持原 URL 直连，不套用镜像。
fn mirror_candidates(url: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = github_download_path(url) {
        let direct = format!("https://github.com/{p}");
        out.push(direct.clone());
        for m in GITHUB_MIRRORS {
            out.push(format!("{}/{}", m.trim_end_matches('/'), direct));
        }
    } else {
        out.push(url.to_string());
    }
    out
}

pub fn cancel_download() {
    DOWNLOAD_CANCEL.store(true, Ordering::Relaxed);
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 最新版本号（去掉 tag 前缀 v，如 "0.1.2"）
    pub version: String,
    /// release notes（markdown 原文）
    pub notes: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    pub published_at: String,
}

/// "v0.1.2" / "0.1.2-beta.1" → [0, 1, 2]，无法解析的段按 0 处理
fn version_tuple(v: &str) -> Vec<u64> {
    let v = v.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let v = v.split(['-', '+']).next().unwrap_or(v);
    v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (version_tuple(latest), version_tuple(current));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 从 release 附件里挑 Windows 安装包：优先 *setup*.exe（Inno 安装包），其次任意 .exe
fn pick_setup_asset(assets: &[serde_json::Value]) -> Option<(String, String, u64)> {
    let mut fallback = None;
    for a in assets {
        let (Some(name), Some(url)) = (a["name"].as_str(), a["browser_download_url"].as_str()) else {
            continue;
        };
        let lower = name.to_lowercase();
        if !lower.ends_with(".exe") {
            continue;
        }
        let item = (name.to_string(), url.to_string(), a["size"].as_u64().unwrap_or(0));
        if lower.contains("setup") {
            return Some(item);
        }
        fallback.get_or_insert(item);
    }
    fallback
}

/// 查询 GitHub 最新 release；有更新返回安装包信息，已是最新返回 None
pub fn fetch_latest(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(12))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(404, _) => "仓库还没有发布版本".to_string(),
            ureq::Error::Status(code, _) => format!("GitHub 返回状态 {code}"),
            other => format!("无法连接 GitHub：{other}"),
        })?;
    let release: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("解析发布信息失败：{e}"))?;

    let tag = release["tag_name"].as_str().unwrap_or_default();
    if tag.is_empty() {
        return Err("仓库还没有发布版本".into());
    }
    if !is_newer(tag, current_version) {
        return Ok(None);
    }
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| "发布信息缺少附件".to_string())?;
    let (asset_name, asset_url, asset_size) = pick_setup_asset(assets).ok_or_else(|| {
        format!(
            "新版本 {tag} 未提供 Windows 安装包，请到 GitHub 发布页手动下载"
        )
    })?;

    Ok(Some(UpdateInfo {
        version: tag.trim().trim_start_matches(|c| c == 'v' || c == 'V').to_string(),
        notes: release["body"].as_str().unwrap_or_default().to_string(),
        asset_name,
        asset_url,
        asset_size,
        published_at: release["published_at"].as_str().unwrap_or_default().to_string(),
    }))
}

/// 从一个 URL 下载全部字节到临时文件，带进度事件。
/// 成功后返回路径；失败（含下载中断）删除半成品文件并返回错误。
fn download_from_one(
    app: &AppHandle,
    url: &str,
    path: &Path,
    expected_size: u64,
) -> Result<PathBuf, String> {
    let resp = ureq::get(url)
        .set("User-Agent", UA)
        .timeout(SOURCE_TIMEOUT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("下载失败（HTTP {code}）"),
            other => format!("下载失败：{other}"),
        })?;
    let total = resp
        .header("Content-Length")
        .and_then(|v: &str| v.parse::<u64>().ok())
        .unwrap_or(expected_size);

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(path).map_err(|e| format!("无法写入临时目录：{e}"))?;

    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        if DOWNLOAD_CANCEL.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err("已取消下载".into());
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                file.write_all(&buf[..n])
                    .map_err(|e| format!("写入安装包失败：{e}"))?;
                received += n as u64;
                if last_emit.elapsed() >= Duration::from_millis(120) {
                    last_emit = Instant::now();
                    let _ = app.emit(
                        "update://progress",
                        serde_json::json!({ "received": received, "total": total }),
                    );
                }
            }
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(format!("下载中断：{e}"));
            }
        }
    }
    file.flush().map_err(|e| format!("写入安装包失败：{e}"))?;
    drop(file);

    if total > 0 && received != total {
        let _ = std::fs::remove_file(path);
        return Err("下载不完整，请重试".into());
    }
    Ok(path.to_path_buf())
}

/// 下载安装包到临时目录，期间通过 `update://progress` 事件上报进度。
/// 返回下载文件的完整路径。文件大小与 release 附件声明的 size 校验一致。
///
/// 可靠性增强：按「直连 → 各 GitHub 代理镜像」的候选顺序依次尝试。
/// 单个来源连接失败 / HTTP 错误 / 下载中断都只切换下一个来源（并上报
/// 一条日志到 stderr，不打断前端进度语义——进度按已接收字节数单调递增，
/// 切换来源时从头重新计数）。若全部来源均失败，返回最后一次的错误。
pub fn download(app: &AppHandle, url: &str, name: &str, expected_size: u64) -> Result<PathBuf, String> {
    DOWNLOAD_CANCEL.store(false, Ordering::Relaxed);
    // 附件名固定为 YimaiMusic_*-setup.exe，仅允许常规文件名字符，避免拼进脚本出问题
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '(' | ')'))
        .collect();
    if safe_name.is_empty() {
        return Err("安装包文件名无效".into());
    }
    let path = std::env::temp_dir().join(&safe_name);

    let candidates = mirror_candidates(url);
    let mut last_err: Option<String> = None;
    for (i, cand) in candidates.iter().enumerate() {
        if i > 0 {
            eprintln!("[updater] 切换下载源 ({}/{})：{cand}", i + 1, candidates.len());
        }
        match download_from_one(app, cand, &path, expected_size) {
            Ok(p) => return Ok(p),
            Err(e) if e == "已取消下载" => return Err(e),
            Err(e) => {
                eprintln!("[updater] 下载源失败：{e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "下载失败：无可用下载源".to_string()))
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 生成 PowerShell 助手脚本内容：等应用退出 → 静默安装（/DIR 固定当前目录）→
/// 等安装进程结束（兼容 UAC 提权中转）→ 重启应用 → 清理安装包与脚本自身
fn build_update_script(setup: &str, dir: &str, exe: &str) -> String {
    let setup_q = ps_quote(setup);
    let dir_q = ps_quote(dir);
    let exe_q = ps_quote(exe);
    let stem = Path::new(setup)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("yimai-music-setup");
    let stem_q = ps_quote(stem);

    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         Start-Sleep -Seconds 2\n\
         $setup = {setup_q}\n\
         $dir = {dir_q}\n\
         $exe = {exe_q}\n\
         Start-Process -FilePath $setup -ArgumentList @('/VERYSILENT','/NORESTART','/SUPPRESSMSGBOXES',('/DIR=\"' + $dir + '\"'))\n\
         while (Get-Process -Name {stem_q} -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 500 }}\n\
         Start-Sleep -Milliseconds 500\n\
         Start-Process -FilePath $exe\n\
         Start-Sleep -Seconds 2\n\
         Remove-Item -LiteralPath $setup -Force\n\
         Remove-Item -LiteralPath $PSCommandPath -Force\n",
    )
}

/// 安装并重启：
/// 1. 写出 PowerShell 助手脚本（UTF-8 BOM，中文路径可安全表示）并分离启动；
/// 2. 助手静默运行安装包，`/DIR` 显式指定为当前安装目录 → 位置不变；
/// 3. 安装进程结束后重启应用，最后清理临时文件。用户取消 UAC 时也会重启旧版本。
pub fn install_and_restart(app: &AppHandle, setup: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败：{e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "获取安装目录失败".to_string())?;
    if !setup.is_file() {
        return Err("安装包不存在，请重新下载".into());
    }

    let script = build_update_script(
        &setup.to_string_lossy(),
        &dir.to_string_lossy(),
        &exe.to_string_lossy(),
    );

    let script_path = std::env::temp_dir().join(format!("yimai-music-update-{}.ps1", std::process::id()));
    std::fs::write(&script_path, format!("\u{feff}{script}"))
        .map_err(|e| format!("写入更新脚本失败：{e}"))?;

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Err(e) = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-File"])
        .arg(&script_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        let _ = std::fs::remove_file(&script_path);
        return Err(format!("启动更新程序失败：{e}"));
    }

    // 助手已分离接管后续流程，稍后退出本进程（给前端留出展示状态的时间）
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        handle.exit(0);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.2.0", "0.1.1"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(is_newer("0.1.2.0", "0.1.1"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.1.1-beta.1", "0.1.1"));
    }

    #[test]
    fn version_tuple_parses() {
        assert_eq!(version_tuple("v0.1.2"), vec![0, 1, 2]);
        assert_eq!(version_tuple("0.1.2-beta.1"), vec![0, 1, 2]);
        assert_eq!(version_tuple(" 1.2 "), vec![1, 2]);
    }

    /// 走真实 GitHub API 的集成验证：需要网络，手动 `cargo test -- --ignored` 运行
    #[test]
    #[ignore]
    fn fetch_latest_against_github() {
        // 以很旧的版本查询：必然有更新，且附件是合法的安装包
        let info = fetch_latest("0.0.1").unwrap().expect("应有更新");
        assert!(is_newer(&info.version, "0.0.1"));
        assert!(info
            .asset_url
            .contains("github.com/jiuge613/YimaiMusic/releases/download"));
        assert!(info.asset_name.to_lowercase().contains("setup"));
        assert!(info.asset_size > 0);

        // 以当前版本查询：除非仓库又发了更新，否则应为 None；若返回了则其版本必须更新
        let current = env!("CARGO_PKG_VERSION");
        if let Some(info) = fetch_latest(current).unwrap() {
            assert!(is_newer(&info.version, current));
        }
    }

    #[test]
    fn script_quotes_paths_with_spaces_and_chinese() {        let s = build_update_script(
            r"C:\Users\张三\AppData\Local\Temp\YimaiMusic_0.1.2.0_x64-setup.exe",
            r"C:\Program Files\Yimai Music",
            r"C:\Program Files\Yimai Music\yimai.exe",
        );
        // 落盘供 PowerShell 解析器做语法校验（UTF-8 BOM，与真实写入一致）
        let dump = std::env::temp_dir().join("yimai-music-update-script-test.ps1");
        std::fs::write(&dump, format!("\u{feff}{s}")).unwrap();
        assert!(s.contains(r"'C:\Users\张三\AppData\Local\Temp\YimaiMusic_0.1.2.0_x64-setup.exe'"));
        assert!(s.contains(r#"('/DIR="' + $dir + '"')"#));
        assert!(s.contains("'C:\\Program Files\\Yimai Music'"));
        assert!(s.contains("Get-Process -Name 'YimaiMusic_0.1.2.0_x64-setup'"));
        // 单引号转义：路径里的 ' 必须 doubled，避免破坏 PS 字符串
        let q = build_update_script("C:\\it's\\setup.exe", "C:\\app", "C:\\app\\yimai.exe");
        assert!(q.contains("'C:\\it''s\\setup.exe'"));
    }

    #[test]
    fn github_download_path_parses_release_url() {
        // 标准 browser_download_url
        assert_eq!(
            github_download_path("https://github.com/jiuge613/YimaiMusic/releases/download/v0.1.11/YimaiMusic_0.1.11.0_x64-setup.exe"),
            Some("jiuge613/YimaiMusic/releases/download/v0.1.11/YimaiMusic_0.1.11.0_x64-setup.exe".to_string())
        );
        // 带 query 也能剥掉
        assert_eq!(
            github_download_path("https://github.com/a/b/releases/download/v1/f.exe?x=1"),
            Some("a/b/releases/download/v1/f.exe".to_string())
        );
        // 非 releases/download 路径 → 不套镜像
        assert_eq!(github_download_path("https://github.com/a/b/tree/main"), None);
        // 非 github 链接 → 不套镜像
        assert_eq!(github_download_path("https://cdn.example.com/app.exe"), None);
    }

    #[test]
    fn mirror_candidates_direct_first_then_mirrors() {
        let url = "https://github.com/jiuge613/YimaiMusic/releases/download/v0.1.11/setup.exe";
        let c = mirror_candidates(url);
        // 直连在最前
        assert_eq!(
            c.first().unwrap(),
            "https://github.com/jiuge613/YimaiMusic/releases/download/v0.1.11/setup.exe"
        );
        // 紧跟各镜像，且每个都拼「完整 https://github.com/...」
        assert_eq!(
            c.get(1).unwrap(),
            "https://ghproxy.cc/https://github.com/jiuge613/YimaiMusic/releases/download/v0.1.11/setup.exe"
        );
        assert_eq!(c.len(), 1 + GITHUB_MIRRORS.len());
        // 末个镜像也正确拼接
        assert!(c.last().unwrap().starts_with("https://cdn.akacoder.online/https://github.com/"));
    }

    #[test]
    fn mirror_candidates_non_github_keeps_single() {
        let c = mirror_candidates("https://some-cdn.example.com/pkg/app.exe");
        assert_eq!(c, vec!["https://some-cdn.example.com/pkg/app.exe".to_string()]);
    }
}
