use std::io::{Read, Write};
use serde_json::json;
use lofty::prelude::*;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::db;
use crate::engine::TrackInfo;
use crate::library;
use crate::lyrics;
use crate::models::*;
use crate::updater;
use crate::AppState;

fn engine_clone(state: &State<AppState>) -> std::sync::Arc<crate::engine::Engine> {
    state.engine.lock().clone()
}

// ---------- 媒体库 ----------

#[tauri::command]
pub async fn list_tracks(state: State<'_, AppState>) -> Result<Vec<TrackMeta>, String> {
    let conn = state.db.lock();
    // 返回库内记录（含 missing 软删除，由前端按视图过滤：资料库隐藏 missing，
    // “我喜欢/最近播放”保留引用）。被用户主动移除的歌曲（removed=1）已在
    // SQL 层过滤，不会出现在任何本地列表。
    Ok(db::list_tracks(&conn))
}

/// 移除一条本地歌曲记录（仅标记 removed，不删磁盘文件）：
/// 资料库不再显示，扫描时不再重新导入；文件保留，既有歌单引用仍可播放。
#[tauri::command]
pub async fn remove_track(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock();
    if !db::mark_track_removed(&conn, id) {
        return Err("歌曲不存在或已移除".into());
    }
    Ok(())
}

/// 最近一次扫描进度快照（WebView 挂起期间 scan://progress 事件丢失，恢复后补发）
#[tauri::command]
pub async fn get_scan_state(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    Ok(state.scan_last.lock().clone())
}

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<Folder>, String> {
    let conn = state.db.lock();
    Ok(db::list_folders(&conn))
}

#[tauri::command]
pub async fn add_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_dir() {
        return Err("该路径不是文件夹".into());
    }
    let norm = library::norm_path(&p);
    {
        let conn = state.db.lock();
        db::add_folder(&conn, &norm)?;
    }
    library::spawn_scan(&app);
    Ok(())
}

#[tauri::command]
pub async fn remove_folder(state: State<'_, AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    {
        let conn = state.db.lock();
        db::remove_folder(&conn, id);
    }
    library::spawn_scan(&app);
    Ok(())
}

#[tauri::command]
pub async fn rescan(app: AppHandle) -> Result<(), String> {
    library::spawn_scan(&app);
    Ok(())
}

/// 在资源管理器中打开文件夹
#[tauri::command]
pub async fn open_folder(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_dir() {
        return Err("该路径不是文件夹".into());
    }
    #[cfg(target_os = "windows")]
    {
        // explorer 已打开该目录时聚焦，否则新开窗口
        std::process::Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("打开资源管理器失败: {e}"))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = p;
        return Err("当前平台不支持".into());
    }
    Ok(())
}

// ---------- 输出设备 ----------

/// 枚举输出设备 + 当前生效的设备名
#[tauri::command]
pub async fn list_output_devices(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let engine = engine_clone(&state);
    let devices = engine.list_output_devices();
    Ok(json!({
        "devices": devices,
        "current": engine.current_device_name(),
        "preference": engine.device_preference(),
    }))
}

/// 切换输出设备；name 为空 = 跟随系统默认（并持久化偏好）
#[tauri::command]
pub async fn set_output_device(
    state: State<'_, AppState>,
    name: Option<String>,
) -> Result<(), String> {
    let engine = engine_clone(&state);
    let pref = name.as_deref().filter(|s| !s.is_empty());
    engine.switch_output_device(pref)?;
    let conn = state.db.lock();
    db::set_setting(
        &conn,
        "output_device",
        pref.unwrap_or(""),
    );
    Ok(())
}

/// 拖拽导入：文件夹加入媒体库，音频文件直接入库
#[tauri::command]
pub async fn drop_paths(
    app: AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<u32, String> {
    let mut added = 0u32;
    let mut need_scan = false;
    for p in paths {
        let pb = std::path::PathBuf::from(&p);
        if !pb.exists() {
            continue;
        }
        if pb.is_dir() {
            let norm = library::norm_path(&pb);
            let conn = state.db.lock();
            if db::add_folder(&conn, &norm).is_ok() {
                added += 1;
                need_scan = true;
            }
        } else if library::is_audio(&pb) {
            let app_data = state.app_data.clone();
            if let Some(t) = library::parse_track(&pb, &app_data) {
                let conn = state.db.lock();
                db::upsert_track(&conn, &t);
                added += 1;
            }
        }
    }
    if need_scan {
        library::spawn_scan(&app);
    }
    Ok(added)
}

// ---------- 歌词 ----------

#[tauri::command]
pub async fn get_lyrics(
    state: State<'_, AppState>,
    track_id: i64,
) -> Result<LyricsPayload, String> {
    let (path, lrc) = {
        let conn = state.db.lock();
        let path = db::get_track_path(&conn, track_id).ok_or("曲目不存在")?;
        let lrc = db::get_lrc_path(&conn, track_id);
        (path, lrc)
    };

    if let Some(lrc_path) = lrc {
        if let Ok(text) = std::fs::read_to_string(&lrc_path) {
            let p = lyrics::parse(&text);
            if !p.lines.is_empty() {
                return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
            }
        }
    }

    // 内嵌歌词
    if let Ok(tagged) = lofty::read_from_path(&path) {
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(text) = tag.get_string(&lofty::tag::ItemKey::Lyrics) {
                let p = lyrics::parse(text);
                if !p.lines.is_empty() {
                    return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
                }
            }
        }
    }
    Ok(LyricsPayload { synced: false, lines: vec![] })
}

/// 备用歌词源（兜底）：按「歌名 + 歌手」搜网易云取最佳匹配，拉取歌词。
/// 各主源（LX 无 lyric 端点 / 平台取词失败 / 本地无内嵌歌词）返回空时，
/// 前端用本命令兜底，解决「无歌词」问题。匹配策略：
/// 歌名归一化相等优先，其次包含关系 + 歌手一致/包含；都取不到返回空。
#[tauri::command]
pub async fn backup_lyric(
    state: State<'_, AppState>,
    title: String,
    artist: String,
) -> Result<LyricsPayload, String> {
    let title = title.trim().to_string();
    let artist = artist.trim().to_string();
    if title.is_empty() {
        return Ok(LyricsPayload { synced: false, lines: vec![] });
    }
    let music_u = netease_cookie(&state);
    let kw = if artist.is_empty() { title.clone() } else { format!("{title} {artist}") };
    let res = match crate::netease::search(&kw, 10, 0, music_u.as_deref()) {
        Ok(r) => r,
        Err(_) => return Ok(LyricsPayload { synced: false, lines: vec![] }),
    };
    let norm = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    let target = norm(&title);
    let target_a = norm(&artist);
    // 打分：0=不匹配；1=歌名包含；3=歌名相等；+2=歌手包含；+4=歌手相等
    let mut best: Option<(i64, i32)> = None; // (song_id, score)
    for s in &res.songs {
        if s.id == 0 {
            continue;
        }
        let name = norm(&s.name);
        let arts: Vec<String> = s
            .artist_str()
            .split(" / ")
            .map(|a| norm(a))
            .collect();
        let mut score = 0i32;
        if !name.is_empty() && name == target {
            score += 3;
        } else if !name.is_empty() && (name.contains(&target) || target.contains(&name)) {
            score += 1;
        }
        if !target_a.is_empty() {
            let hit_exact = arts.iter().any(|a| a == &target_a);
            let hit_part = arts
                .iter()
                .any(|a| !a.is_empty() && (a.contains(&target_a) || target_a.contains(a.as_str())));
            if hit_exact {
                score += 4;
            } else if hit_part {
                score += 2;
            }
        }
        // 要求至少歌名层面命中（歌名相等或歌手有命中）才采信，避免拿错歌
        if name == target || (score >= 1 && !target_a.is_empty()) {
            if best.map(|(_, b)| score > b).unwrap_or(true) {
                best = Some((s.id, score));
            }
        }
    }
    let Some((id, _)) = best else {
        return Ok(LyricsPayload { synced: false, lines: vec![] });
    };
    // 逐字优先，失败回落行级 LRC；都没有时返回空（不阻断）
    if let Ok(Some(lrc)) = crate::netease::lyric_yrc(id, music_u.as_deref()) {
        let p = lyrics::parse(&lrc);
        if !p.lines.is_empty() {
            return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
        }
    }
    if let Ok(Some(lrc)) = crate::netease::lyric(id, music_u.as_deref()) {
        let p = lyrics::parse(&lrc);
        if !p.lines.is_empty() {
            return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
        }
    }
    Ok(LyricsPayload { synced: false, lines: vec![] })
}

/// 读取 GD音乐台兜底源配置：{ enabled, base, attribution }
#[tauri::command]
pub async fn gd_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let conn = state.db.lock();
    let enabled = db::get_setting(&conn, "gd_enabled")
        .map(|s| s == "true")
        .unwrap_or(false);
    let base = db::get_setting(&conn, "gd_base")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::gdstudio::DEFAULT_BASE.to_string());
    Ok(json!({
        "enabled": enabled,
        "base": base,
        "attribution": crate::gdstudio::ATTRIBUTION,
    }))
}

/// 开启/关闭 GD音乐台歌词兜底，并可同时改接口地址。
/// 开启 = 用户确认接受对方 CC BY-NC 条款（见 gdstudio.rs 模块注释）。
#[tauri::command]
pub async fn set_gd_fallback(
    state: State<'_, AppState>,
    enabled: bool,
    base: Option<String>,
) -> Result<serde_json::Value, String> {
    if let Some(b) = base.as_deref() {
        // 存之前先校验，避免把内网地址落库后每次调用都被拒
        let norm = crate::gdstudio::normalize_base(b)?;
        let conn = state.db.lock();
        db::set_setting(&conn, "gd_base", &norm);
    }
    {
        let conn = state.db.lock();
        db::set_setting(&conn, "gd_enabled", if enabled { "true" } else { "false" });
    }
    // 直接回读拼装，不去 await 另一个命令（State 跨 await 会让 future 失去 Send）
    let conn = state.db.lock();
    let cur_base = db::get_setting(&conn, "gd_base")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::gdstudio::DEFAULT_BASE.to_string());
    Ok(json!({
        "enabled": enabled,
        "base": cur_base,
        "attribution": crate::gdstudio::ATTRIBUTION,
    }))
}

/// GD音乐台歌词兜底：本地/平台/网易云备用源都没歌词时才走到这里。
/// 任何失败都静默返回空 payload（不弹错、不阻断播放）。
#[tauri::command]
pub async fn gd_lyric(
    state: State<'_, AppState>,
    title: String,
    artist: String,
) -> Result<LyricsPayload, String> {
    let empty = LyricsPayload { synced: false, lines: vec![] };
    let (enabled, base) = {
        let conn = state.db.lock();
        (
            db::get_setting(&conn, "gd_enabled")
                .map(|s| s == "true")
                .unwrap_or(false),
            db::get_setting(&conn, "gd_base")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| crate::gdstudio::DEFAULT_BASE.to_string()),
        )
    };
    if !enabled {
        return Ok(empty);
    }
    let title = title.trim();
    if title.is_empty() {
        return Ok(empty);
    }
    let artist = artist.trim();
    let kw = if artist.is_empty() {
        title.to_string()
    } else {
        format!("{title} {artist}")
    };

    // 阻塞调用整体挪到 spawn_blocking：两次 HTTP + 解析，别占着异步运行时
    let base2 = base.clone();
    let (t2, a2) = (title.to_string(), artist.to_string());
    let found = tauri::async_runtime::spawn_blocking(move || {
        let songs = crate::gdstudio::search(&base2, "netease", &kw, 10);
        let Some(hit) = crate::gdstudio::pick(&songs, &t2, &a2) else {
            return String::new();
        };
        // lyric_id 优先（官方语义），缺失退回 id
        let id = if hit.lyric_id.trim().is_empty() {
            hit.id.clone()
        } else {
            hit.lyric_id.clone()
        };
        let src = if hit.source.trim().is_empty() {
            "netease".to_string()
        } else {
            hit.source.clone()
        };
        crate::gdstudio::lyric(&base2, &src, &id)
    })
    .await
    .map_err(|e| e.to_string())
    .unwrap_or_default();

    if found.trim().is_empty() {
        return Ok(empty);
    }
    let p = lyrics::parse(&found);
    if p.lines.is_empty() {
        return Ok(empty);
    }
    Ok(LyricsPayload { synced: p.synced, lines: p.lines })
}

// ---------- 喜欢 / 统计 ----------

#[tauri::command]
pub async fn like_track(state: State<'_, AppState>, id: i64, liked: bool) -> Result<(), String> {
    let conn = state.db.lock();
    db::like_track(&conn, id, liked);
    Ok(())
}

// ---------- 播放列表 ----------

#[tauri::command]
pub async fn list_playlists(state: State<'_, AppState>) -> Result<Vec<Playlist>, String> {
    let conn = state.db.lock();
    Ok(db::list_playlists(&conn))
}

#[tauri::command]
pub async fn create_playlist(
    state: State<'_, AppState>,
    name: String,
) -> Result<i64, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("播放列表名称不能为空".into());
    }
    let conn = state.db.lock();
    db::create_playlist(&conn, &name)
}

#[tauri::command]
pub async fn delete_playlist(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock();
    db::delete_playlist(&conn, id);
    Ok(())
}

#[tauri::command]
pub async fn rename_playlist(
    state: State<'_, AppState>,
    id: i64,
    name: String,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::rename_playlist(&conn, id, &name);
    Ok(())
}

/// 播放列表手动排序（侧边栏长按拖动）：按 id 序列重写 sort_pos
#[tauri::command]
pub async fn reorder_playlists(state: State<'_, AppState>, ids: Vec<i64>) -> Result<(), String> {
    let conn = state.db.lock();
    db::reorder_playlists(&conn, &ids);
    Ok(())
}

#[tauri::command]
pub async fn add_to_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    track_id: i64,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::add_to_playlist(&conn, playlist_id, track_id)
}

#[tauri::command]
pub async fn remove_from_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    track_id: i64,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::remove_from_playlist(&conn, playlist_id, track_id);
    Ok(())
}

// ---------- 在线音源 ----------

#[tauri::command]
pub async fn list_sources(state: State<'_, AppState>) -> Result<Vec<SourceItem>, String> {
    let conn = state.db.lock();
    Ok(db::list_sources(&conn))
}

#[tauri::command]
pub async fn add_source(
    state: State<'_, AppState>,
    url: String,
    title: Option<String>,
) -> Result<i64, String> {
    let url = url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("音源地址必须以 http:// 或 https:// 开头".into());
    }
    if url.contains(".m3u8") {
        return Err("暂不支持 m3u8/HLS，请使用音频文件直链".into());
    }
    let conn = state.db.lock();
    if let Some(item) = db::list_sources(&conn).into_iter().find(|s| s.url == url) {
        return Ok(item.id);
    }
    db::add_source(&conn, &url, title.as_deref().unwrap_or(""))
}

#[tauri::command]
pub async fn delete_source(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock();
    db::delete_source(&conn, id);
    Ok(())
}

// ---------- 音源管理（LX 兼容脚本 / 网络接口音源） ----------

use crate::lxsource;

/// 音源管理列表
#[tauri::command]
pub async fn lx_list_sources(state: State<'_, AppState>) -> Result<Vec<LxSourceItem>, String> {
    let conn = state.db.lock();
    Ok(db::lx_list_sources(&conn))
}

/// 导入本地音源脚本（选文件读出的内容 / 粘贴的脚本文本）：
/// 静态解析契约后入库；解析失败返回带原因的中文错误。
///
/// `base_url` / `api_mode` 为可选回填：混淆脚本（取链基址运行时解码、静态无解）
/// 由前端在 webview 内执行脚本取出基址与协议模式后传入，跳过静态 `extract_base`。
#[tauri::command]
pub async fn lx_add_script_source(
    state: State<'_, AppState>,
    content: String,
    base_url: Option<String>,
    api_mode: Option<String>,
) -> Result<serde_json::Value, String> {
    let parsed =
        lxsource::parse_script_impl(&content, base_url.as_deref(), api_mode.as_deref())?;
    let platforms_json = serde_json::to_string(&parsed.platforms).unwrap_or_else(|_| "[]".into());
    let conn = state.db.lock();
    let (id, created) = db::lx_add_source(
        &conn,
        "script",
        &parsed.name,
        &parsed.base_url,
        &content,
        &platforms_json,
        &parsed.api_mode,
    )?;
    Ok(json!({ "id": id, "created": created, "name": parsed.name, "baseUrl": parsed.base_url, "apiMode": parsed.api_mode }))
}

/// 添加网络音源（音源站根地址或 .js 订阅链接）：拉取 + 探测校验后入库。
#[tauri::command]
pub async fn lx_add_network_source(
    state: State<'_, AppState>,
    url: String,
) -> Result<serde_json::Value, String> {
    let outcome = lxsource::probe_source(&url)?;
    let kind = if outcome.via == "音源脚本订阅" { "script" } else { "network" };
    let platforms_json =
        serde_json::to_string(&outcome.platforms).unwrap_or_else(|_| "[]".into());
    let conn = state.db.lock();
    let (id, created) = db::lx_add_source(
        &conn,
        kind,
        &outcome.name,
        &outcome.base_url,
        &outcome.origin,
        &platforms_json,
        "",
    )?;
    Ok(json!({
        "id": id, "created": created, "kind": kind,
        "name": outcome.name, "baseUrl": outcome.base_url,
        "via": outcome.via, "platforms": outcome.platforms,
    }))
}

/// 启用 / 禁用
#[tauri::command]
pub async fn lx_set_source_enabled(
    state: State<'_, AppState>,
    id: i64,
    enabled: bool,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::lx_set_enabled(&conn, id, enabled)
}

/// 删除
#[tauri::command]
pub async fn lx_delete_source(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock();
    db::lx_delete_source(&conn, id);
    Ok(())
}

/// 读取本地脚本文件文本（导入对话框选出的路径）。限制 1 MB、仅文本脚本。
#[tauri::command]
pub async fn lx_read_script_file(path: String) -> Result<String, String> {
    let lower = path.to_lowercase();
    if !lower.ends_with(".js") && !lower.ends_with(".txt") && !lower.ends_with(".json") {
        return Err("请选择音源脚本文件（.js）".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| format!("读取文件失败: {e}"))?;
    if meta.len() > 1024 * 1024 {
        return Err("脚本文件超过 1 MB，疑似不是音源脚本".into());
    }
    std::fs::read_to_string(&path).map_err(|e| format!("读取文件失败: {e}"))
}

/// 测试取链：真实调用 {base}/url.php 拉一次直链（不下载音频，只校验返回）。
/// 供 UI「测试」按钮与后续播放链路集成使用。
#[tauri::command]
pub async fn lx_resolve_url(
    state: State<'_, AppState>,
    source_id: i64,
    platform: String,
    song_id: String,
    quality: Option<String>,
    extra: Option<String>,
) -> Result<String, String> {
    let (base, api_mode) = {
        let conn = state.db.lock();
        let item = db::lx_list_sources(&conn)
            .into_iter()
            .find(|s| s.id == source_id)
            .ok_or("音源不存在")?;
        if !item.enabled {
            return Err("该音源已停用，请先在设置中启用".into());
        }
        (item.base_url, item.api_mode)
    };
    lxsource::music_url(
        &base,
        &platform,
        &song_id,
        quality.as_deref().unwrap_or("320k"),
        extra.as_deref(),
        &api_mode,
    )
}

// ---------- 排行榜（音源搜索 + 取链播放） ----------

/// 选源：指定 id 优先；否则按平台能力匹配；都没有则取第一个启用的源
fn pick_source(
    sources: &[LxSourceItem],
    source_id: Option<i64>,
    platform: &str,
) -> Option<LxSourceItem> {
    if let Some(id) = source_id {
        return sources.iter().find(|s| s.id == id).cloned();
    }
    if !platform.is_empty() {
        if let Some(s) = sources
            .iter()
            .find(|s| s.platforms.iter().any(|p| p.code == platform))
        {
            return Some(s.clone());
        }
    }
    sources.first().cloned()
}

/// 确定取链/搜索用的平台代码（未指定时取音源声明的第一个平台，兜底 wy）
fn resolve_platform(source: &LxSourceItem, platform: &str) -> String {
    if !platform.is_empty() {
        return platform.to_string();
    }
    source
        .platforms
        .first()
        .map(|p| p.code.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "wy".to_string())
}

/// 在音源声明的音质里挑一个：优先请求值，其次 320k / 128k，最后第一个
fn pick_quality(source: &LxSourceItem, platform: &str, requested: &str) -> String {
    let avail: Vec<String> = source
        .platforms
        .iter()
        .find(|p| p.code == platform)
        .map(|p| p.qualitys.clone())
        .unwrap_or_default();
    if avail.is_empty() {
        return if requested.is_empty() {
            "320k".to_string()
        } else {
            requested.to_string()
        };
    }
    if avail.iter().any(|q| q == requested) {
        return requested.to_string();
    }
    for want in ["320k", "192k", "128k", "flac"] {
        if avail.iter().any(|q| q == want) {
            return want.to_string();
        }
    }
    avail.first().cloned().unwrap_or_else(|| "320k".to_string())
}

/// 内置平台搜索（音源未提供搜索接口时的兜底，保证排行榜始终可用）
fn builtin_search(
    state: &State<AppState>,
    platform: &str,
    keyword: &str,
    limit: i64,
) -> Result<Vec<LxSearchSong>, String> {
    let code = match platform {
        "wy" | "netease" => "wy",
        "tx" | "qq" => "tx",
        // 未知平台（kw/mg/…）与未指定平台：酷狗免登录通道最稳
        _ => "kg",
    };
    match code {
        "wy" => {
            let music_u = netease_cookie(state);
            let r = crate::netease::search(keyword, limit, 0, music_u.as_deref())?;
            Ok(r.songs
                .into_iter()
                .map(|s| {
                    // 先算完借用类字段再移动 s.name，避免部分移动后借用的编译错误
                    let artist = s.artist_str();
                    let album = s.album_name();
                    let duration_ms = s.duration_ms().max(0) as u64;
                    LxSearchSong {
                        id: s.id.to_string(),
                        title: s.name,
                        artist,
                        album,
                        duration_ms,
                        platform: "wy".into(),
                        extra: String::new(),
                    }
                })
                .collect())
        }
        "tx" => {
            let songs = crate::qq::search(keyword, limit, 1)?;
            Ok(songs
                .into_iter()
                .map(|s| LxSearchSong {
                    id: s.id.clone(),
                    title: s.name,
                    artist: s.singer,
                    album: s.album,
                    duration_ms: s.duration_ms,
                    platform: "tx".into(),
                    extra: s.media_mid,
                })
                .collect())
        }
        _ => {
            let songs = crate::kugou::search(keyword, 1)?;
            Ok(songs
                .into_iter()
                .take(limit.max(1) as usize)
                .map(|s| LxSearchSong {
                    id: s.id.clone(),
                    title: s.name,
                    artist: s.singer,
                    album: s.album,
                    duration_ms: s.duration_ms,
                    platform: "kg".into(),
                    extra: s.id,
                })
                .collect())
        }
    }
}

/// 排行榜搜索：优先走已接入音源的 search.php，失败/无结果时回退内置平台搜索。
/// 返回 `{ via, sourceId, sourceName, platform, songs, sourceError? }`。
#[tauri::command]
pub async fn lx_search(
    state: State<'_, AppState>,
    source_id: Option<i64>,
    platform: Option<String>,
    keyword: String,
    limit: Option<i64>,
) -> Result<serde_json::Value, String> {
    let kw = keyword.trim().to_string();
    if kw.is_empty() {
        return Err("请输入搜索关键词".into());
    }
    let limit = limit.unwrap_or(30).clamp(1, 50);
    let want_platform = platform.unwrap_or_default();
    let sources = {
        let conn = state.db.lock();
        db::lx_enabled_sources(&conn)
    };
    let picked = pick_source(&sources, source_id, &want_platform);

    let mut src_err: Option<String> = None;
    let mut used_platform = want_platform.clone();
    if let Some(s) = &picked {
        let code = resolve_platform(s, &want_platform);
        used_platform = code.clone();
        match lxsource::search_songs(&s.base_url, &code, &kw, limit) {
            Ok(songs) if !songs.is_empty() => {
                let songs: Vec<LxSearchSong> = songs
                    .into_iter()
                    .map(|mut x| {
                        x.platform = code.clone();
                        x
                    })
                    .collect();
                return Ok(json!({
                    "via": "音源搜索",
                    "sourceId": s.id,
                    "sourceName": s.name,
                    "platform": code,
                    "songs": songs,
                }));
            }
            // 音源返回空列表：交给内置兜底，最终以"无结果"呈现
            Ok(_) => {}
            Err(e) => {
                eprintln!("[lxsource] 音源搜索失败（{}）: {e}", s.name);
                src_err = Some(e);
            }
        }
    }

    match builtin_search(&state, &want_platform, &kw, limit) {
        Ok(songs) => {
            let used_platform = songs.first().map(|s| s.platform.clone()).unwrap_or(used_platform);
            Ok(json!({
                "via": if picked.is_some() { "内置平台搜索" } else { "内置平台搜索（未接入音源）" },
                "sourceId": picked.as_ref().map(|s| s.id),
                "sourceName": picked.as_ref().map(|s| s.name.clone()),
                "platform": used_platform,
                "songs": songs,
                "sourceError": src_err,
            }))
        }
        Err(e) => Err(match src_err {
            Some(se) => format!("{se}；内置平台搜索同样失败：{e}"),
            None => e,
        }),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LxPlaySongReq {
    pub source_id: i64,
    pub platform: String,
    pub song_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// 期望音质（320k / flac …）；留空时按设置与音源声明自动选
    #[serde(default)]
    pub quality: Option<String>,
    /// 取链扩展上下文（酷狗 hash / QQ media_mid）
    #[serde(default)]
    pub extra: Option<String>,
}

/// 未接入音源时的播放回退：直接走内置平台取链（wy→网易云、tx→QQ、kg→酷狗）。
/// 保证排行榜在未配置音源时也能播（酷狗匿名可用；网易云/QQ 需已登录）。
fn builtin_play_song(state: &State<AppState>, req: &LxPlaySongReq) -> Result<(), String> {
    let quality = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let (url, kind, rid, media_mid, label) = match req.platform.as_str() {
        "kg" => {
            let (u, ext) = crate::kugou::song_url(&req.song_id, false)?;
            (u, "kugou", req.song_id.clone(), String::new(), quality_tag(&ext, 128))
        }
        "wy" => {
            let id: i64 = req
                .song_id
                .parse()
                .map_err(|_| "网易云曲目 ID 无效，无法取链".to_string())?;
            let music_u = netease_cookie(state);
            let (u, br, ext) = crate::netease::song_url(id, music_u.as_deref(), &quality)?
                .ok_or_else(|| "该歌曲暂无可播放链接（可能需要登录或有效 VIP 权益）".to_string())?;
            (u, "netease", req.song_id.clone(), String::new(), quality_tag(&ext, br))
        }
        "tx" => {
            let (musicid, musickey) = qq_credential(state)?;
            let (u, ext, kbps) = crate::qq::song_url(
                &req.song_id,
                req.extra.as_deref().unwrap_or(""),
                &musicid,
                &musickey,
                &quality,
                false,
            )?;
            (
                u,
                "qq",
                req.song_id.clone(),
                req.extra.clone().unwrap_or_default(),
                quality_tag(&ext, kbps),
            )
        }
        _ => {
            return Err(
                "该曲目来自音源平台，当前未接入音源无法取链；请在设置 → 音源管理 中添加音源"
                    .into(),
            )
        }
    };
    {
        let conn = state.db.lock();
        db::record_play_online(
            &conn,
            kind,
            &rid,
            &req.title,
            &req.artist,
            &req.album,
            &req.cover,
            req.duration_ms as i64,
            &media_mid,
            false,
        );
    }
    let info = TrackInfo {
        id: None,
        kind: kind.into(),
        path: String::new(),
        title: req.title.clone(),
        artist: req.artist.clone(),
        album: req.album.clone(),
        cover: req.cover.clone(),
        duration_ms: req.duration_ms,
        nid: None,
        qid: None,
        kgid: None,
        quality: Some(label),
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    engine_clone(state).play_url(url, info)
}

/// 排行榜曲目播放：用已接入音源取链 → 走统一的在线播放链路（缓存 + 解码）。
///
/// 这是"音源管理"与播放链路的集成点：与网易云/QQ/酷狗在线播放共用
/// `play_url`，因此进度、歌词、SMTC、最近播放等行为完全一致。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn lx_play_song(
    state: State<'_, AppState>,
    source_id: Option<i64>,
    platform: Option<String>,
    song_id: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    cover: Option<String>,
    duration_ms: Option<u64>,
    quality: Option<String>,
    extra: Option<String>,
) -> Result<(), String> {
    // 前端以扁平参数调用（与 lx_search / lx_resolve_url 一致），这里收敛成内部结构体。
    #[allow(unused_mut)]
    let mut req = LxPlaySongReq {
        source_id: source_id.unwrap_or(0),
        platform: platform.unwrap_or_default(),
        song_id: song_id.unwrap_or_default(),
        title: title.unwrap_or_default(),
        artist: artist.unwrap_or_default(),
        album: album.unwrap_or_default(),
        cover: cover.unwrap_or_default(),
        duration_ms: duration_ms.unwrap_or(0),
        quality: quality.filter(|q| !q.trim().is_empty()),
        extra: extra.filter(|e| !e.trim().is_empty()),
    };
    if req.song_id.trim().is_empty() {
        return Err("歌曲 ID 无效，无法取链".into());
    }
    // 兜底：未接入音源且未指定平台时走酷狗（匿名可直接取链），避免空参数直接报错
    if req.platform.trim().is_empty() && req.source_id <= 0 {
        req.platform = "kg".to_string();
    }
    // 未接入音源（sourceId <= 0）：回退内置平台取链，保证页面开箱可用
    if req.source_id <= 0 {
        return builtin_play_song(&state, &req);
    }
    let (base, src_name, platforms, api_mode) = {
        let conn = state.db.lock();
        let item = db::lx_list_sources(&conn)
            .into_iter()
            .find(|s| s.id == req.source_id)
            .ok_or("音源不存在，请在设置 → 音源管理中重新添加")?;
        if !item.enabled {
            return Err("该音源已停用，请先在设置中启用".into());
        }
        (item.base_url, item.name, item.platforms, item.api_mode)
    };
    let platform = {
        let src_item = LxSourceItem {
            id: req.source_id,
            kind: "network".into(),
            name: src_name.clone(),
            base_url: base.clone(),
            origin: String::new(),
            platforms: platforms.clone(),
            enabled: true,
            api_mode: String::new(),
            created_at: 0,
        };
        resolve_platform(&src_item, &req.platform)
    };
    // 音质：请求值 → 设置映射 → 音源声明
    let setting = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let by_setting = lx_quality(&setting);
    let want = req.quality.clone().filter(|q| !q.is_empty()).unwrap_or_else(|| by_setting.to_string());
    let quality = {
        let src_item = LxSourceItem {
            id: req.source_id,
            kind: "network".into(),
            name: src_name.clone(),
            base_url: base.clone(),
            origin: String::new(),
            platforms,
            enabled: true,
            api_mode: String::new(),
            created_at: 0,
        };
        pick_quality(&src_item, &platform, &want)
    };

    let url = lxsource::music_url(
        &base,
        &platform,
        &req.song_id,
        &quality,
        req.extra.as_deref(),
        &api_mode,
    )?;

    // 最近播放：只有能映射回内置平台的曲目才记录（否则前端无法二次播放）
    let kind = match platform.as_str() {
        "wy" => Some("netease"),
        "tx" => Some("qq"),
        "kg" => Some("kugou"),
        _ => None,
    };
    if let Some(k) = kind {
        let conn = state.db.lock();
        db::record_play_online(
            &conn,
            k,
            &req.song_id,
            &req.title,
            &req.artist,
            &req.album,
            &req.cover,
            req.duration_ms as i64,
            req.extra.as_deref().unwrap_or(""),
            false,
        );
    }

    let info = TrackInfo {
        id: None,
        kind: "url".into(),
        path: String::new(),
        title: if req.title.is_empty() {
            "未知曲目".to_string()
        } else {
            req.title
        },
        artist: req.artist,
        album: req.album,
        cover: req.cover,
        duration_ms: req.duration_ms,
        nid: None,
        qid: None,
        kgid: None,
        quality: Some(quality.to_uppercase()),
        // 仅当来自真实 LX 音源（source_id > 0）才携带身份，便于回查歌词；
        // 内置平台回退播放（source_id <= 0）不带，避免无效回查。
        lx_source_id: if req.source_id > 0 { Some(req.source_id) } else { None },
        lx_platform: if req.source_id > 0 { Some(platform.clone()) } else { None },
        lx_song_id: if req.source_id > 0 { Some(req.song_id.clone()) } else { None },
    };
    engine_clone(&state).play_url(url, info)
}

/// LX 音源歌词：`GET {base}/lyric.php?source=&id=` → 解析 LRC。
///
/// 与 `lx_play_song` 共用音源库里的 base_url；前端凭播放时携带的 LX 身份
/// （source_id / platform / song_id）调用，使 LX 导入音源的曲目也能显示歌词。
#[tauri::command]
pub async fn lx_lyric(
    state: State<'_, AppState>,
    source_id: i64,
    platform: String,
    song_id: String,
) -> Result<LyricsPayload, String> {
    let base = {
        let conn = state.db.lock();
        let item = db::lx_list_sources(&conn)
            .into_iter()
            .find(|s| s.id == source_id)
            .ok_or("音源不存在，请在设置 → 音源管理中重新添加")?;
        if !item.enabled {
            return Err("该音源已停用，请先在设置中启用".into());
        }
        item.base_url
    };
    let text = lxsource::lyric(&base, &platform, &song_id)?;
    let p = lyrics::parse(&text);
    Ok(LyricsPayload { synced: p.synced, lines: p.lines })
}

/// 下载 LX 音源曲目到保存目录（写标签入库，资料库可见）。
///
/// 复用与 `lx_play_song` 完全一致的取链链路（含 api_mode 分流），因此标准协议与
/// 自定义 v1 端点（混淆脚本）的曲目都能下载。下载完成后解析标签入库，并把该曲目
/// 标记为在线已下载，保存目录纳入资料库扫描。
#[tauri::command]
pub async fn lx_download(
    app: AppHandle,
    req: LxDownloadReq,
) -> Result<String, String> {
    // 同 download_online：整段是阻塞 IO（取链 HTTP + 落盘 + 写标签），
    // 不能占用 async 运行时的 worker
    tauri::async_runtime::spawn_blocking(move || lx_download_blocking(app, req))
        .await
        .map_err(|e| format!("下载任务异常终止: {e}"))?
}

fn lx_download_blocking(app: AppHandle, req: LxDownloadReq) -> Result<String, String> {
    let state = app.state::<AppState>();
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err("歌曲标题为空".into());
    }
    let (base, api_mode, src_name, platforms) = {
        let conn = state.db.lock();
        let item = db::lx_list_sources(&conn)
            .into_iter()
            .find(|s| s.id == req.source_id)
            .ok_or("音源不存在，请在设置 → 音源管理中重新添加")?;
        if !item.enabled {
            return Err("该音源已停用，请先在设置中启用".into());
        }
        (item.base_url, item.api_mode, item.name, item.platforms)
    };

    // 平台/音质抉择与播放链路一致
    let platform = {
        let src_item = LxSourceItem {
            id: req.source_id,
            kind: "network".into(),
            name: src_name.clone(),
            base_url: base.clone(),
            origin: String::new(),
            platforms: platforms.clone(),
            enabled: true,
            api_mode: api_mode.clone(),
            created_at: 0,
        };
        resolve_platform(&src_item, &req.platform)
    };
    let setting = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let by_setting = lx_quality(&setting);
    let quality = {
        let src_item = LxSourceItem {
            id: req.source_id,
            kind: "network".into(),
            name: src_name.clone(),
            base_url: base.clone(),
            origin: String::new(),
            platforms,
            enabled: true,
            api_mode: api_mode.clone(),
            created_at: 0,
        };
        pick_quality(&src_item, &platform, by_setting)
    };

    let url = lxsource::music_url(
        &base,
        &platform,
        &req.song_id,
        &quality,
        req.extra.as_deref(),
        &api_mode,
    )?;

    // 扩展名：从直链路径推断，失败兜底 mp3
    let ext = ext_from_url(&url);

    // 下载到保存目录（写 .part → 校验 → 改名，不覆盖已有曲目）
    let dir = save_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建保存目录失败: {e}"))?;
    let artist = sanitize_filename(&req.artist);
    let stem = format!(
        "{} - {}",
        if artist.is_empty() { "Unknown" } else { &artist },
        sanitize_filename(&title)
    );
    let dest = download_to_dir(&app, "lx", &url, &dir, &stem, &ext, &title, None)?;
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // 歌词：尝试回查，失败静默（部分源无 lyric 端点）
    let lyrics = lxsource::lyric(&base, &platform, &req.song_id).ok();
    write_tags(&dest, &title, &req.artist, &req.album, &req.cover, lyrics.as_deref());

    // 解析入库 + 标记在线已下载 + 保存目录纳入扫描
    let mut track = crate::library::parse_track(&dest, &state.app_data).ok_or("解析歌曲失败")?;
    if track.duration == 0.0 && req.duration_ms > 0 {
        track.duration = req.duration_ms as f64 / 1000.0;
    }
    {
        let conn = state.db.lock();
        db::upsert_track(&conn, &track);
        db::mark_online_downloaded(&conn, "lx", &format!("{}-{}", req.source_id, req.song_id));
        let _ = db::add_folder(&conn, &dir.to_string_lossy());
    }
    Ok(name)
}

/// 从直链 URL 推断文件扩展名（取最后一个路径段的扩展；无则兜底 mp3）
fn ext_from_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let seg = path.rsplit('/').next().unwrap_or("");
    if let Some(pos) = seg.rfind('.') {
        let e = &seg[pos + 1..];
        if !e.is_empty() && e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()) {
            return e.to_lowercase();
        }
    }
    "mp3".to_string()
}

// ---------- 播放控制 ----------

#[tauri::command]
pub async fn play_track(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let meta = {
        let conn = state.db.lock();
        db::get_track(&conn, id).ok_or("曲目不存在")?
    };
    {
        let conn = state.db.lock();
        db::record_play(&conn, id);
    }
    let local_quality = if meta.bit_depth >= 16 && meta.sample_rate >= 44100 {
        format!(
            "{}kHz/{}bit",
            meta.sample_rate / 1000,
            meta.bit_depth.max(16)
        )
    } else if meta.bitrate > 0 {
        format!("{}kbps", meta.bitrate / 1000)
    } else {
        String::new()
    };
    let info = TrackInfo {
        id: Some(meta.id),
        kind: "track".into(),
        path: meta.path,
        title: meta.title,
        artist: meta.artist,
        album: meta.album,
        cover: meta.cover,
        duration_ms: (meta.duration * 1000.0) as u64,
        nid: None,
        qid: None,
        kgid: None,
        quality: (!local_quality.is_empty()).then_some(local_quality),
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    engine_clone(&state).play_file(info)
}

#[tauri::command]
pub async fn play_source(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let item = {
        let conn = state.db.lock();
        db::get_source(&conn, id).ok_or("音源不存在")?
    };
    let info = TrackInfo {
        id: None,
        kind: "url".into(),
        path: String::new(),
        title: if item.title.is_empty() {
            item.url.split('/').next_back().unwrap_or("在线音源").to_string()
        } else {
            item.title.clone()
        },
        artist: "在线音源".into(),
        album: String::new(),
        cover: String::new(),
        duration_ms: 0,
        nid: None,
        qid: None,
        kgid: None,
        quality: None,
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    engine_clone(&state).play_url(item.url, info)
}

// ---------- 网易云在线曲库 ----------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NeteasePlayReq {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub duration_ms: u64,
}

/// 音质标签（在线曲目播放栏徽标）：FLAC → 无损；≥320kbps → HQ；其余 → 标准
fn quality_tag(ext: &str, br_kbps: i64) -> String {
    if ext.eq_ignore_ascii_case("flac") {
        "无损".to_string()
    } else if br_kbps >= 320 {
        "HQ".to_string()
    } else {
        "标准".to_string()
    }
}

fn netease_cookie(state: &AppState) -> Option<String> {
    let conn = state.db.lock();
    db::get_setting(&conn, "netease_music_u").filter(|s| !s.is_empty())
}

/// 应用内音质档位 → LX 音源协议的 quality 字符串。
/// LX 的 url.php 接受 128k/192k/256k/320k/flac；pick_quality 会在音源
/// 不声明该档时自动降到它支持的最近一档，所以这里可以直接透传。
fn lx_quality(quality: &str) -> &'static str {
    match quality {
        "standard" => "128k",
        "medium" => "192k",
        "higher" => "256k",
        "lossless" => "flac",
        _ => "320k",
    }
}

#[tauri::command]
pub async fn netease_search(
    state: State<'_, AppState>,
    keyword: String,
    offset: Option<i64>,
) -> Result<crate::netease::NetSearchResult, String> {
    let music_u = netease_cookie(&state);
    crate::netease::search(&keyword, 30, offset.unwrap_or(0), music_u.as_deref())
}

#[tauri::command]
pub async fn netease_play(
    app: AppHandle,
    state: State<'_, AppState>,
    track: NeteasePlayReq,
) -> Result<(), String> {
    let music_u = netease_cookie(&state);
    let quality = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let (url, br, ext) = crate::netease::song_url(track.id, music_u.as_deref(), &quality)?
        .ok_or_else(|| {
            "该歌曲暂无可播放链接（可能需要登录，或需要有效 VIP 权益）".to_string()
        })?;
    let quality_label = quality_tag(&ext, br);
    // 记录到“最近播放”（在线曲目元数据轻量入库）
    {
        let conn = state.db.lock();
        db::record_play_online(
            &conn,
            "netease",
            &track.id.to_string(),
            &track.title,
            &track.artist,
            &track.album,
            &track.cover,
            track.duration_ms as i64,
            "",
            false,
        );
    }
    let info = TrackInfo {
        id: None,
        kind: "netease".into(),
        path: String::new(),
        title: track.title,
        artist: track.artist,
        album: track.album,
        cover: track.cover,
        duration_ms: track.duration_ms,
        nid: Some(track.id),
        qid: None,
        kgid: None,
        quality: Some(quality_label),
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    let _ = app; // 事件由引擎发出
    engine_clone(&state).play_url(url, info)
}

#[tauri::command]
pub async fn netease_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let conn = state.db.lock();
    let music_u = db::get_setting(&conn, "netease_music_u").unwrap_or_default();
    let nickname = db::get_setting(&conn, "netease_nickname").unwrap_or_default();
    Ok(json!({
        "loggedIn": !music_u.is_empty(),
        "nickname": nickname,
    }))
}

#[tauri::command]
pub async fn netease_qr_create() -> Result<serde_json::Value, String> {
    let (key, qr) = crate::netease::qr_create()?;
    Ok(json!({ "key": key, "qr": qr }))
}

#[tauri::command]
pub async fn netease_qr_check(
    state: State<'_, AppState>,
    key: String,
) -> Result<serde_json::Value, String> {
    let r = crate::netease::qr_check(&key)?;
    if r.status == "success" {
        if let Some(music_u) = &r.music_u {
            let conn = state.db.lock();
            db::set_setting(&conn, "netease_music_u", music_u);
            if let Some(nick) = &r.nickname {
                db::set_setting(&conn, "netease_nickname", nick);
            }
            if let Some(uid) = &r.user_id {
                db::set_setting(&conn, "netease_uid", &uid.to_string());
            }
        }
    }
    Ok(json!({
        "status": r.status,
        "nickname": r.nickname,
    }))
}

#[tauri::command]
pub async fn netease_like_list(state: State<'_, AppState>) -> Result<Vec<i64>, String> {
    let (uid, music_u) = {
        let conn = state.db.lock();
        (
            db::get_setting(&conn, "netease_uid").unwrap_or_default(),
            db::get_setting(&conn, "netease_music_u").unwrap_or_default(),
        )
    };
    if uid.is_empty() || music_u.is_empty() {
        return Ok(vec![]);
    }
    let uid: i64 = uid.parse().map_err(|_| "账号 ID 无效".to_string())?;
    crate::netease::like_list(uid, &music_u)
}

#[tauri::command]
pub async fn netease_like(
    state: State<'_, AppState>,
    id: i64,
    like: bool,
) -> Result<(), String> {
    let music_u = netease_cookie(&state).ok_or("未登录网易云账号")?;
    crate::netease::like(id, like, &music_u)
}

#[tauri::command]
pub async fn netease_lyric(
    state: State<'_, AppState>,
    id: i64,
) -> Result<LyricsPayload, String> {
    let music_u = netease_cookie(&state);
    // 优先逐字歌词（yrc）：染色推进贴合实际演唱节奏；无则回落行级
    if let Ok(Some(enhanced)) = crate::netease::lyric_yrc(id, music_u.as_deref()) {
        let p = lyrics::parse(&enhanced);
        if p.synced {
            return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
        }
    }
    let lrc = crate::netease::lyric(id, music_u.as_deref())?;
    let text = lrc.unwrap_or_default();
    if text.is_empty() {
        return Ok(LyricsPayload { synced: false, lines: vec![] });
    }
    let p = lyrics::parse(&text);
    Ok(LyricsPayload { synced: p.synced, lines: p.lines })
}

#[tauri::command]
pub async fn netease_logout(state: State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock();
    db::set_setting(&conn, "netease_music_u", "");
    db::set_setting(&conn, "netease_nickname", "");
    // uid 也必须清：netease_user_playlists 会优先用它取歌单，
    // 留着旧 uid 会让"已登出"的用户仍能拉到上一个账号的歌单
    db::set_setting(&conn, "netease_uid", "");
    Ok(())
}

// ---------- QQ 音乐在线曲库 ----------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QqPlayReq {
    pub songmid: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_mid: String,
    #[serde(default)]
    pub media_mid: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// 搜索结果里的 VIP 标志，用于播放失败分类（权益不足 vs 真无版权）
    #[serde(default)]
    pub vip: bool,
}

fn qq_credential(state: &AppState) -> Result<(String, String), String> {
    let (musicid, musickey) = {
        let conn = state.db.lock();
        (
            db::get_setting(&conn, "qq_musicid").unwrap_or_default(),
            db::get_setting(&conn, "qq_musickey").unwrap_or_default(),
        )
    };
    if musicid.is_empty() || musickey.is_empty() {
        return Err("未登录 QQ 音乐账号，无法获取播放链接，请先扫码登录".into());
    }
    Ok((musicid, musickey))
}

#[tauri::command]
pub async fn qq_search(
    keyword: String,
    page: Option<i64>,
) -> Result<serde_json::Value, String> {
    let songs = crate::qq::search(&keyword, 30, page.unwrap_or(1))?;
    Ok(json!({ "songs": songs }))
}

#[tauri::command]
pub async fn qq_play(
    state: State<'_, AppState>,
    track: QqPlayReq,
) -> Result<(), String> {
    let (musicid, musickey) = qq_credential(&state)?;
    let quality = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let (url, ext, kbps) =
        crate::qq::song_url(&track.songmid, &track.media_mid, &musicid, &musickey, &quality, track.vip)?;
    // 用接口实际返回的档位，QQ 无 192/256 档时会显示真实的 320K
    let quality_label = quality_tag(&ext, kbps);
    // 封面优先用数据库存的完整 URL（歌单导入时已写入），缺失再拼 album_mid
    let cover = {
        let conn = state.db.lock();
        db::get_online_cover(&conn, "qq", &track.songmid).unwrap_or_else(|| {
            if track.album_mid.is_empty() {
                String::new()
            } else {
                format!(
                    "https://y.gtimg.cn/music/photo_new/T002R300x300M000{}.jpg",
                    track.album_mid
                )
            }
        })
    };
    let info = TrackInfo {
        id: None,
        kind: "qq".into(),
        path: String::new(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        cover: cover.clone(),
        duration_ms: track.duration_ms,
        nid: None,
        qid: Some(track.songmid.clone()),
        kgid: None,
        quality: Some(quality_label),
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    // 记录到“最近播放”（在线曲目元数据轻量入库）
    {
        let conn = state.db.lock();
        db::record_play_online(
            &conn,
            "qq",
            &track.songmid,
            &track.title,
            &track.artist,
            &track.album,
            &cover,
            track.duration_ms as i64,
            &track.media_mid,
            false,
        );
    }
    engine_clone(&state).play_url(url, info)
}

#[tauri::command]
pub async fn qq_lyric(state: State<'_, AppState>, songmid: String) -> Result<LyricsPayload, String> {
    // 优先逐字歌词（QRC，需登录）；失败回落匿名行级接口
    if let Ok((musicid, musickey)) = qq_credential(&state) {
        if let Ok(Some(enhanced)) = crate::qq::lyric_qrc(&songmid, &musicid, &musickey) {
            let p = lyrics::parse(&enhanced);
            if p.synced {
                return Ok(LyricsPayload { synced: p.synced, lines: p.lines });
            }
        }
    }
    let text = crate::qq::lyric(&songmid)?.unwrap_or_default();
    if text.is_empty() {
        return Ok(LyricsPayload { synced: false, lines: vec![] });
    }
    let p = lyrics::parse(&text);
    Ok(LyricsPayload { synced: p.synced, lines: p.lines })
}

// ---------- 酷狗音乐在线曲库（匿名，免登录） ----------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KgPlayReq {
    pub hash: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// 搜索结果里的付费标志，用于播放失败分类
    #[serde(default)]
    pub vip: bool,
}

#[tauri::command]
pub async fn kugou_search(
    keyword: String,
    page: Option<i64>,
) -> Result<serde_json::Value, String> {
    let songs = crate::kugou::search(&keyword, page.unwrap_or(1))?;
    Ok(json!({ "songs": songs }))
}

#[tauri::command]
pub async fn kugou_play(state: State<'_, AppState>, track: KgPlayReq) -> Result<(), String> {
    let (url, ext) = crate::kugou::song_url(&track.hash, track.vip)?;
    let quality_label = quality_tag(&ext, 128);
    // 封面：数据库存的完整 URL 优先（收藏/最近播放已入库），缺失用搜索带的
    let cover = {
        let conn = state.db.lock();
        db::get_online_cover(&conn, "kugou", &track.hash).unwrap_or_default()
    };
    let cover = if cover.is_empty() { track.cover.clone() } else { cover };
    let info = TrackInfo {
        id: None,
        kind: "kugou".into(),
        path: String::new(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        cover: cover.clone(),
        duration_ms: track.duration_ms,
        nid: None,
        qid: None,
        kgid: Some(track.hash.clone()),
        quality: Some(quality_label),
        lx_source_id: None,
        lx_platform: None,
        lx_song_id: None,
    };
    // 记录到“最近播放”（在线曲目元数据轻量入库）
    {
        let conn = state.db.lock();
        db::record_play_online(
            &conn,
            "kugou",
            &track.hash,
            &track.title,
            &track.artist,
            &track.album,
            &cover,
            track.duration_ms as i64,
            "",
            track.vip,
        );
    }
    engine_clone(&state).play_url(url, info)
}

#[tauri::command]
pub async fn kugou_lyric(state: State<'_, AppState>, hash: String) -> Result<LyricsPayload, String> {
    // 入库时已记录的酷狗歌词直接走远端；本地缓存散布在播放缓存之外，简化为直取
    let _ = &state;
    let text = crate::kugou::lyric(&hash)?;
    let text = match text {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(LyricsPayload { synced: false, lines: vec![] }),
    };
    let p = lyrics::parse(&text);
    Ok(LyricsPayload { synced: p.synced, lines: p.lines })
}

#[tauri::command]
pub async fn qq_qr_create() -> Result<serde_json::Value, String> {
    let (qrsig, qr) = crate::qq::qr_create()?;
    Ok(json!({ "qrsig": qrsig, "qr": qr }))
}

#[tauri::command]
pub async fn qq_qr_check(
    state: State<'_, AppState>,
    qrsig: String,
) -> Result<serde_json::Value, String> {
    let r = crate::qq::qr_check(&qrsig)?;
    if r.status == "success" {
        if let (Some(musicid), Some(musickey)) = (&r.musicid, &r.musickey) {
            let conn = state.db.lock();
            db::set_setting(&conn, "qq_musicid", musicid);
            db::set_setting(&conn, "qq_musickey", musickey);
            // 拉取“我喜欢/收藏”夹需要加密 uin（登录响应里带，错过就没了）
            if let Some(euin) = &r.encrypt_uin {
                db::set_setting(&conn, "qq_encrypt_uin", euin);
            }
            if let Some(nick) = &r.nickname {
                db::set_setting(&conn, "qq_nickname", nick);
            }
        }
    }
    Ok(json!({ "status": r.status, "nickname": r.nickname }))
}

#[tauri::command]
pub async fn qq_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let conn = state.db.lock();
    let musicid = db::get_setting(&conn, "qq_musicid").unwrap_or_default();
    let nickname = db::get_setting(&conn, "qq_nickname").unwrap_or_default();
    Ok(json!({ "loggedIn": !musicid.is_empty(), "nickname": nickname }))
}

#[tauri::command]
pub async fn qq_logout(state: State<'_, AppState>) -> Result<(), String> {
    let conn = state.db.lock();
    db::set_setting(&conn, "qq_musicid", "");
    db::set_setting(&conn, "qq_musickey", "");
    db::set_setting(&conn, "qq_encrypt_uin", "");
    db::set_setting(&conn, "qq_nickname", "");
    Ok(())
}

// ---------- 在线歌曲收藏到本地 / 歌单导入 ----------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineSaveReq {
    pub kind: String, // netease | qq
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover_url: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub media_mid: String,
}

/// LX 音源下载请求（凭播放时携带的音源身份取链后下载）
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LxDownloadReq {
    pub source_id: i64,
    pub platform: String,
    pub song_id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub extra: Option<String>,
}

fn save_dir(state: &AppState) -> std::path::PathBuf {
    let conn = state.db.lock();
    let custom = db::get_setting(&conn, "save_dir").unwrap_or_default();
    if custom.is_empty() {
        state.app_data.join("下载音乐")
    } else {
        std::path::PathBuf::from(custom)
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 单个下载的体积上限。超过说明取链返回的不是正常音频（常见是接口 JSON /
/// 网页被当成了直链），继续写只会产出被截断的坏文件。
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// 目标文件已存在时追加 " (1)"、" (2)" …，绝不覆盖已有曲目。
/// 原先直接 File::create 目标名，同名同扩展名的文件会被静默截断。
fn unique_dest(dir: &std::path::Path, stem: &str, ext: &str) -> std::path::PathBuf {
    let first = if ext.is_empty() {
        dir.join(stem)
    } else {
        dir.join(format!("{stem}.{ext}"))
    };
    if !first.exists() {
        return first;
    }
    for i in 1..1000 {
        let cand = if ext.is_empty() {
            dir.join(format!("{stem} ({i})"))
        } else {
            dir.join(format!("{stem} ({i}).{ext}"))
        };
        if !cand.exists() {
            return cand;
        }
    }
    first
}

/// 下载音频到保存目录：先写 .part，全部校验通过再改名成成品。
/// 修掉三处问题：
/// - 直接写目标名会静默覆盖已有曲目 → unique_dest 改名避让
/// - 失败时半截文件留在保存目录，下次扫描会把它当坏曲目收进资料库 → 失败清理
/// - 超过体积上限时 take() 会"正常"结束，产出被截断的伪成功文件 → 显式判错
fn download_to_dir(
    app: &AppHandle,
    kind: &str,
    url: &str,
    dir: &std::path::Path,
    stem: &str,
    ext: &str,
    progress_key: &str,
    task_id: Option<&str>,
) -> Result<std::path::PathBuf, String> {
    let dest = unique_dest(dir, stem, ext);
    let part = dest.with_extension(format!("{ext}.part"));
    let mut emitted = false;

    let received: u64 = {
        let mut last_emit = std::time::Instant::now();
        let run = (|| -> Result<u64, String> {
            let (total, reader) = http_get_for(kind, url)?;
            // +1 是为了能察觉"刚好超过上限"：取上限本身再读一次
            let mut reader = reader.take(MAX_DOWNLOAD_BYTES + 1);
            let mut file =
                std::fs::File::create(&part).map_err(|e| format!("创建文件失败: {e}"))?;
            let mut buf = [0u8; 64 * 1024];
            let mut received: u64 = 0;
            loop {
                let n = reader.read(&mut buf).map_err(|e| format!("下载失败: {e}"))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .map_err(|e| format!("写入文件失败: {e}"))?;
                received += n as u64;
                if received > MAX_DOWNLOAD_BYTES {
                    return Err(format!(
                        "文件超过 {}MB 上限，已中止（取链返回的可能是非音频内容）",
                        MAX_DOWNLOAD_BYTES / 1024 / 1024
                    ));
                }
                if last_emit.elapsed() >= std::time::Duration::from_millis(300) {
                    last_emit = std::time::Instant::now();
                    emitted = true;
                    let pct = if total > 0 {
                        ((received as f64 / total as f64) * 100.0) as u64
                    } else {
                        0
                    };
                    // 顺带把进度写进任务表，下载管理页才能显示多条并发任务的进度
                    if let Some(tid) = task_id {
                        let st = app.state::<AppState>();
                        let conn = st.db.lock();
                        db::update_download_progress(&conn, tid, received as i64, total as i64);
                    }
                    let _ = app.emit(
                        "download://progress",
                        json!({ "url": progress_key, "taskId": task_id, "received": received, "total": total, "pct": pct.min(99), "done": false }),
                    );
                }
            }
            drop(file);
            // 服务端给了 content-length 就必须下满，否则是断流而非正常结束
            if total > 0 && received < total {
                return Err(format!(
                    "下载不完整（收到 {received} 字节，应为 {total} 字节），请重试"
                ));
            }
            Ok(received)
        })();
        match run {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                // 发过进度就先把进度条收掉，避免卡在 99%
                if emitted {
                    let _ = app.emit(
                        "download://progress",
                        json!({ "url": progress_key, "done": true }),
                    );
                }
                return Err(e);
            }
        }
    };

    std::fs::rename(&part, &dest).map_err(|e| format!("保存文件失败: {e}"))?;
    let _ = app.emit(
        "download://progress",
        json!({ "url": progress_key, "received": received, "pct": 100, "done": true }),
    );
    Ok(dest)
}

/// 收藏在线歌曲到“我喜欢”（轻量引用，不下载；播放时按权益取链接）
#[tauri::command]
pub async fn like_online(
    state: State<'_, AppState>,
    kind: String,
    rid: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    cover: Option<String>,
    duration_ms: Option<i64>,
    media_mid: Option<String>,
    vip: Option<bool>,
    like: Option<bool>,
) -> Result<(), String> {
    let conn = state.db.lock();
    let like = like.unwrap_or(true);
    if like {
        db::upsert_online_track(
            &conn,
            &kind,
            &rid,
            &title,
            &artist.unwrap_or_default(),
            &album.unwrap_or_default(),
            &cover.unwrap_or_default(),
            duration_ms.unwrap_or(0),
            &media_mid.unwrap_or_default(),
            vip.unwrap_or(false),
        );
        db::like_online_track(&conn, &kind, &rid);
    } else {
        db::unlike_online_track(&conn, &kind, &rid);
    }
    Ok(())
}

/// 下载在线歌曲到保存目录（写标签入库，资料库可见）
#[tauri::command]
pub async fn download_online(
    app: AppHandle,
    req: OnlineSaveReq,
) -> Result<String, String> {
    // 全流程都是阻塞 IO（多次 HTTP + 整文件落盘 + lofty 解析 + 写标签），
    // 放 async 命令里会占住运行时工作线程数分钟，期间其他命令无法派发。
    // 整个包进 spawn_blocking；State 不是 'static，闭包里改用 AppHandle 取状态。
    tauri::async_runtime::spawn_blocking(move || download_online_blocking(app, req))
        .await
        .map_err(|e| format!("下载任务异常终止: {e}"))?
}

fn download_online_blocking(app: AppHandle, req: OnlineSaveReq) -> Result<String, String> {
    download_online_blocking_with_task(app, req, None)
}

/// 与 `download_online_blocking` 相同，但额外把进度写进下载任务表。
/// 传 None 时行为与原来完全一致（即时下载，不进任务表）。
fn download_online_blocking_with_task(
    app: AppHandle,
    req: OnlineSaveReq,
    task_id: Option<String>,
) -> Result<String, String> {
    let state = app.state::<AppState>();
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err("歌曲标题为空".into());
    }

    // 1) 按当前音质取播放链接
    let quality = {
        let conn = state.db.lock();
        db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string())
    };
    let (url, ext) = match req.kind.as_str() {
        "netease" => {
            let id: i64 = req.id.parse().map_err(|_| "网易云歌曲 ID 无效".to_string())?;
            let music_u = netease_cookie(&state);
            let (u, _br, ext) = crate::netease::song_url(id, music_u.as_deref(), &quality)?
                .ok_or("该歌曲暂无可播放链接")?;
            (u, ext)
        }
        "qq" => {
            let (musicid, musickey) = qq_credential(&state)?;
            let (u, ext, _kbps) = crate::qq::song_url(
                &req.id,
                &req.media_mid,
                &musicid,
                &musickey,
                &quality,
                true,
            )?;
            (u, ext)
        }
        "kugou" => crate::kugou::song_url(&req.id, true)?,
        _ => return Err("未知音源类型".into()),
    };

    // 2) 下载到保存目录（写 .part → 校验 → 改名，不覆盖已有曲目）
    let dir = save_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建保存目录失败: {e}"))?;
    let artist = sanitize_filename(&req.artist);
    let stem = format!(
        "{} - {}",
        if artist.is_empty() { "Unknown" } else { &artist },
        sanitize_filename(&title)
    );
    let dest = download_to_dir(&app, &req.kind, &url, &dir, &stem, &ext, &title, task_id.as_deref())?;
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // 3) 取歌词并写标签（失败静默）
    let lyrics = match req.kind.as_str() {
        "netease" => crate::netease::lyric(
            req.id.parse::<i64>().unwrap_or(0),
            netease_cookie(&state).as_deref(),
        )
        .ok()
        .flatten(),
        "qq" => crate::qq::lyric(&req.id).ok().flatten(),
        _ => None,
    };
    write_tags(&dest, &title, &req.artist, &req.album, &req.cover_url, lyrics.as_deref());

    // 4) 解析入库 + 标记已下载 + 保存目录纳入扫描
    let mut track = crate::library::parse_track(&dest, &state.app_data).ok_or("解析歌曲失败")?;
    // 文件标签缺失时长时，用前端传入的元数据时长兜底
    if track.duration == 0.0 && req.duration_ms > 0 {
        track.duration = req.duration_ms as f64 / 1000.0;
    }
    {
        let conn = state.db.lock();
        db::upsert_track(&conn, &track);
        db::mark_online_downloaded(&conn, &req.kind, &req.id);
        let _ = db::add_folder(&conn, &dir.to_string_lossy());
        // 下载管理页需要真实文件路径（"打开所在位置"与删除文件都靠它）
        if let Some(tid) = task_id.as_deref() {
            db::set_download_status(
                &conn,
                tid,
                "done",
                "",
                &dest.to_string_lossy(),
            );
        }
    }
    Ok(name)
}

// ---------- 下载管理 ----------

/// 下载任务 id：平台 + 曲目 id，同一首歌重复下载复用同一行
fn dl_task_id(kind: &str, song_id: &str) -> String {
    format!("{kind}:{song_id}")
}

/// 排队中的任务。首次使用时惰性启动固定数量的 worker，
/// 避免一次点十几首把网络与磁盘打满。
fn dl_queue() -> &'static parking_lot::Mutex<std::collections::VecDeque<OnlineSaveReq>> {
    static Q: std::sync::OnceLock<parking_lot::Mutex<std::collections::VecDeque<OnlineSaveReq>>> =
        std::sync::OnceLock::new();
    Q.get_or_init(|| parking_lot::Mutex::new(std::collections::VecDeque::new()))
}

fn ensure_workers(app: &AppHandle) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        for _ in 0..2 {
            let app = app.clone();
            let _ = std::thread::Builder::new()
                .name("yimai-downloader".into())
                .spawn(move || loop {
                    let job = { dl_queue().lock().pop_front() };
                    let Some(req) = job else {
                        // 空队列：歇一会儿再看，避免忙等烧 CPU
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        continue;
                    };
                    run_download_job(app.clone(), req);
                });
        }
    });
}

/// 执行一个下载任务并回写任务状态
fn run_download_job(app: AppHandle, req: OnlineSaveReq) {
    let tid = dl_task_id(&req.kind, &req.id);
    {
        let st = app.state::<AppState>();
        let conn = st.db.lock();
        db::set_download_status(&conn, &tid, "downloading", "", "");
    }
    let _ = app.emit("download://task", json!({ "taskId": tid.clone(), "status": "downloading" }));

    // 直接调用即可：worker 是普通 std 线程（不是运行时线程），
    // 再套一层 block_on + spawn_blocking 没有意义，还会引入
    // "在运行时线程内 block_on 会 panic" 的风险。
    let res = download_online_blocking_with_task(app.clone(), req, Some(tid.clone()));

    let (status, err) = match &res {
        Ok(_) => ("done", String::new()),
        Err(e) => ("failed", e.clone()),
    };
    {
        let st = app.state::<AppState>();
        let conn = st.db.lock();
        db::set_download_status(&conn, &tid, status, &err, "");
    }
    let _ = app.emit(
        "download://task",
        json!({ "taskId": tid.clone(), "status": status, "error": err }),
    );
}

/// 读取应用日志（诊断用）。release 版无控制台，出问题靠这个文件。
#[tauri::command]
pub async fn read_app_log(state: State<'_, AppState>) -> Result<String, String> {
    let p = state.app_data.join("yimai.log");
    std::fs::read_to_string(&p).map_err(|e| format!("暂无日志（{}）：{e}", p.display()))
}

/// 在资源管理器里定位日志文件
#[tauri::command]
pub async fn open_app_log(state: State<'_, AppState>) -> Result<(), String> {
    let dir = state.app_data.clone();
    std::process::Command::new("explorer")
        .arg(dir.to_string_lossy().to_string())
        .spawn()
        .map_err(|e| format!("打开目录失败: {e}"))?;
    Ok(())
}

/// 列出下载任务。status 为空返回全部。
#[tauri::command]
pub async fn list_downloads(
    state: State<'_, AppState>,
    status: String,
) -> Result<Vec<crate::db::DownloadTask>, String> {
    let conn = state.db.lock();
    Ok(db::list_download_tasks(&conn, &status))
}

/// 加入下载队列。返回任务 id（失败会落在任务表的 failed 状态里，可重试）。
#[tauri::command]
pub async fn enqueue_download(
    app: AppHandle,
    state: State<'_, AppState>,
    req: OnlineSaveReq,
) -> Result<String, String> {
    let tid = dl_task_id(&req.kind, &req.id);
    let already_done = {
        let conn = state.db.lock();
        db::get_download_task(&conn, &tid)
            .map(|t| t.status == "done")
            .unwrap_or(false)
    };
    if already_done {
        return Ok(tid); // 已下完，不重复排队
    }
    {
        let conn = state.db.lock();
        let task = crate::db::DownloadTask {
            id: tid.clone(),
            kind: req.kind.clone(),
            song_id: req.id.clone(),
            media_mid: req.media_mid.clone(),
            title: req.title.clone(),
            artist: req.artist.clone(),
            album: req.album.clone(),
            cover: req.cover_url.clone(),
            size: 0,
            received: 0,
            status: "queued".into(),
            error: String::new(),
            file_path: String::new(),
            created_at: db::now_secs(),
            finished_at: 0,
        };
        db::upsert_download_task(&conn, &task);
    }
    ensure_workers(&app);
    dl_queue().lock().push_back(req);
    let _ = app.emit("download://task", json!({ "taskId": tid.clone(), "status": "queued" }));
    Ok(tid)
}

/// 重试：任务重置为 queued 并重新入队
#[tauri::command]
pub async fn retry_download(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let req = {
        let conn = state.db.lock();
        let Some(t) = db::get_download_task(&conn, &task_id) else {
            return Err("任务不存在".into());
        };
        db::set_download_status(&conn, &task_id, "queued", "", "");
        OnlineSaveReq {
            kind: t.kind,
            id: t.song_id,
            title: t.title,
            artist: t.artist,
            album: t.album,
            cover_url: t.cover,
            duration_ms: 0,
            media_mid: t.media_mid,
        }
    };
    ensure_workers(&app);
    dl_queue().lock().push_back(req);
    let _ = app.emit("download://task", json!({ "taskId": task_id.clone(), "status": "queued" }));
    Ok(())
}

/// 删除单个任务。delete_file 为 true 时同时删掉磁盘上的音频。
#[tauri::command]
pub async fn delete_download(
    state: State<'_, AppState>,
    task_id: String,
    delete_file: bool,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::delete_download_task(&conn, &task_id, delete_file);
    Ok(())
}

/// 清空某个状态的全部任务。返回被清理的记录数。
#[tauri::command]
pub async fn clear_downloads(
    state: State<'_, AppState>,
    status: String,
    delete_files: bool,
) -> Result<usize, String> {
    let conn = state.db.lock();
    let before = db::list_download_tasks(&conn, &status).len();
    db::clear_download_tasks(&conn, &status, delete_files);
    Ok(before)
}

/// 在资源管理器中定位已下载的音频
#[tauri::command]
pub async fn open_download_location(
    state: State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let path = {
        let conn = state.db.lock();
        db::get_download_task(&conn, &task_id)
            .map(|t| t.file_path)
            .unwrap_or_default()
    };
    if path.is_empty() || !std::path::Path::new(&path).exists() {
        return Err("文件不存在或已移动".into());
    }
    std::process::Command::new("explorer")
        .arg(format!("/select,\"{path}\""))
        .spawn()
        .map_err(|e| format!("打开资源管理器失败: {e}"))?;
    Ok(())
}

/// 导出任务列表为 CSV，直接落到下载目录并返回文件路径。
/// 刻意不在前端拼路径：前端拿到的 default 与 save_dir() 在用户设过自定义
/// 目录时并不一致，交给后端自己写既不会拼错，也没有任意路径写入的口子。
#[tauri::command]
pub async fn export_downloads(
    app: AppHandle,
    state: State<'_, AppState>,
    status: String,
) -> Result<String, String> {
    let csv = {
        let conn = state.db.lock();
        build_downloads_csv(&conn, &status)?
    };
    let dir = save_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建下载目录失败: {e}"))?;
    let name = format!(
        "YimaiMusic-下载记录-{}.csv",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let path = dir.join(&name);
    std::fs::write(&path, csv).map_err(|e| format!("写入失败: {e}"))?;
    let _ = app.emit("download://exported", json!({ "path": path.to_string_lossy() }));
    Ok(path.to_string_lossy().into_owned())
}

fn build_downloads_csv(conn: &rusqlite::Connection, status: &str) -> Result<String, String> {
    let tasks = db::list_download_tasks(conn, status);
    if tasks.is_empty() {
        return Err("没有可导出的记录".into());
    }
    let esc = |t: &str| -> String {
        if t.contains([',', '"', '\n']) {
            format!("\"{}\"", t.replace('"', "\"\""))
        } else {
            t.to_string()
        }
    };
    let mut out = String::from("歌名,歌手,专辑,大小(字节),状态,完成时间\n");
    for t in &tasks {
        let time = if t.finished_at > 0 {
            fmt_unix(t.finished_at)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            esc(&t.title),
            esc(&t.artist),
            esc(&t.album),
            t.size,
            t.status,
            time
        ));
    }
    Ok(out)
}

/// unix 秒 → "YYYY-MM-DD HH:MM"（为一个格式化函数引入 chrono 不划算）
fn fmt_unix(ts: i64) -> String {
    let mut d = ts / 86400;
    let secs = ts.rem_euclid(86400);
    let mut year = 1970i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let yd = if leap { 366 } else { 365 };
        if d < yd {
            break;
        }
        d -= yd;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let ml = [
        31i64,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && d >= ml[month] {
        d -= ml[month];
        month += 1;
    }
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        year,
        month + 1,
        d + 1,
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// “我喜欢”列表：在线条目部分
#[tauri::command]
pub async fn liked_online_list(
    state: State<'_, AppState>,
) -> Result<Vec<crate::models::PlaylistEntryMeta>, String> {
    let conn = state.db.lock();
    Ok(db::liked_online_list(&conn)
        .into_iter()
        .map(|e| crate::models::PlaylistEntryMeta {
            rowid: 0,
            kind: e.kind,
            track_id: None,
            online_id: Some(e.online_id),
            title: e.title,
            artist: e.artist,
            album: e.album,
            cover: e.cover,
            duration: e.duration,
            media_mid: e.media_mid,
            vip: e.vip,
            last_played: 0,
            liked_at: e.liked_at,
        })
        .collect())
}

/// “最近播放”的在线曲目部分（最近播放过的，按时间倒序）
#[tauri::command]
pub async fn recent_online_list(
    state: State<'_, AppState>,
) -> Result<Vec<crate::models::PlaylistEntryMeta>, String> {
    let conn = state.db.lock();
    Ok(db::recent_online_list(&conn, 100)
        .into_iter()
        .map(|e| crate::models::PlaylistEntryMeta {
            rowid: 0,
            kind: e.kind,
            track_id: None,
            online_id: Some(e.online_id),
            title: e.title,
            artist: e.artist,
            album: e.album,
            cover: e.cover,
            duration: e.duration,
            media_mid: e.media_mid,
            vip: e.vip,
            last_played: e.last_played,
            liked_at: 0,
        })
        .collect())
}

/// 获取下载保存目录（custom 为空时用 default）
#[tauri::command]
pub async fn save_dir_get(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let conn = state.db.lock();
    let custom = db::get_setting(&conn, "save_dir").unwrap_or_default();
    Ok(json!({
        "dir": custom,
        "default": state.app_data.join("下载音乐").to_string_lossy(),
    }))
}

#[tauri::command]
pub async fn save_dir_set(state: State<'_, AppState>, dir: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&dir);
    if !p.is_dir() {
        return Err("该路径不是文件夹".into());
    }
    let conn = state.db.lock();
    db::set_setting(&conn, "save_dir", &dir);
    Ok(())
}

fn http_get_for(kind: &str, url: &str) -> Result<(u64, impl std::io::Read), String> {
    // 连接与读取分段超时：整体超时会在大文件下载中途掐断连接
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30));
    if kind == "qq" {
        if let Some(p) = crate::qq::system_proxy() {
            builder = builder.proxy(p);
        }
    }
    let agent = builder.build();
    let mut req = agent.get(url).set("User-Agent", "Mozilla/5.0");
    if kind == "qq" {
        req = req
            .set("Referer", "https://y.qq.com/")
            .set("User-Agent", crate::qq::UA);
    }
    let resp = req.call().map_err(|e| format!("下载失败: {e}"))?;
    let total: u64 = resp
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Ok((total, resp.into_reader()))
}

/// 给下载的音频写标签（标题/艺术家/专辑/封面/歌词）
fn write_tags(
    path: &std::path::Path,
    title: &str,
    artist: &str,
    album: &str,
    cover_url: &str,
    lyrics: Option<&str>,
) {
    let _ = (|| -> Result<(), String> {
        use lofty::prelude::*;
        let mut tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
        let tag_type = tagged.file_type().primary_tag_type();
        {
            let tag = match tagged.primary_tag_mut() {
                Some(t) => t,
                None => {
                    tagged.insert_tag(lofty::tag::Tag::new(tag_type));
                    tagged.primary_tag_mut().ok_or("无主标签")?
                }
            };
            tag.insert_text(lofty::tag::ItemKey::TrackTitle, title.to_string());
            if !artist.is_empty() {
                tag.insert_text(lofty::tag::ItemKey::TrackArtist, artist.to_string());
            }
            if !album.is_empty() {
                tag.insert_text(lofty::tag::ItemKey::AlbumTitle, album.to_string());
            }
            if let Some(lrc) = lyrics.filter(|l| !l.trim().is_empty()) {
                tag.insert_text(lofty::tag::ItemKey::Lyrics, lrc.to_string());
            }
            if !cover_url.is_empty() {
                if let Ok(resp) = ureq::get(cover_url)
                    .set("User-Agent", "Mozilla/5.0")
                    .timeout(std::time::Duration::from_secs(15))
                    .call()
                {
                    let mut data = Vec::new();
                    if resp.into_reader().read_to_end(&mut data).is_ok() && !data.is_empty() {
                        let mime = if cover_url.contains(".png") {
                            lofty::picture::MimeType::Png
                        } else {
                            lofty::picture::MimeType::Jpeg
                        };
                        let pic = lofty::picture::Picture::new_unchecked(
                            lofty::picture::PictureType::CoverFront,
                            Some(mime),
                            None,
                            data,
                        );
                        tag.push_picture(pic);
                    }
                }
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        tagged
            .save_to(&mut file, lofty::config::WriteOptions::default())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
}

#[tauri::command]
pub async fn add_online_to_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    kind: String,
    rid: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    cover: Option<String>,
    duration_ms: Option<i64>,
    media_mid: Option<String>,
    vip: Option<bool>,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::upsert_online_track(
        &conn,
        &kind,
        &rid,
        &title,
        &artist.unwrap_or_default(),
        &album.unwrap_or_default(),
        &cover.unwrap_or_default(),
        duration_ms.unwrap_or(0),
        &media_mid.unwrap_or_default(),
        vip.unwrap_or(false),
    );
    db::add_online_to_playlist(&conn, playlist_id, &kind, &rid)?;
    Ok(())
}

#[tauri::command]
pub async fn remove_playlist_entry(state: State<'_, AppState>, rowid: i64) -> Result<(), String> {
    let conn = state.db.lock();
    db::remove_playlist_entry(&conn, rowid);
    Ok(())
}

/// 手动排序持久化：“资料库 / 我喜欢”整份顺序（全量覆盖）。
/// list: "library" | "liked"；keys 为行标识序列：
/// 本地 "track:<id>"、网易云 "netease:<rid>"、QQ "qq:<rid>"
#[tauri::command]
pub async fn save_manual_order(
    state: State<'_, AppState>,
    list: String,
    keys: Vec<String>,
) -> Result<(), String> {
    if !matches!(list.as_str(), "library" | "liked") {
        return Err("未知排序列表".into());
    }
    let conn = state.db.lock();
    db::save_manual_order(&conn, &list, &keys);
    Ok(())
}

/// 读取“资料库 / 我喜欢”的手动排序（row key → 序号；无记录的行序号为 0）
#[tauri::command]
pub async fn get_manual_order(
    state: State<'_, AppState>,
    list: String,
) -> Result<std::collections::HashMap<String, i64>, String> {
    if !matches!(list.as_str(), "library" | "liked") {
        return Err("未知排序列表".into());
    }
    let conn = state.db.lock();
    Ok(db::manual_order_map(&conn, &list))
}

/// 播放列表条目手动排序：按 rowid 序列重写 position（全量覆盖）
#[tauri::command]
pub async fn reorder_playlist(
    state: State<'_, AppState>,
    playlist_id: i64,
    rowids: Vec<i64>,
) -> Result<(), String> {
    let conn = state.db.lock();
    db::reorder_playlist(&conn, playlist_id, &rowids);
    Ok(())
}

#[tauri::command]
pub async fn netease_user_playlists(
    state: State<'_, AppState>,
) -> Result<Vec<crate::models::UserPlaylistMeta>, String> {
    let (uid, music_u) = {
        let conn = state.db.lock();
        (
            db::get_setting(&conn, "netease_uid").unwrap_or_default(),
            db::get_setting(&conn, "netease_music_u").unwrap_or_default(),
        )
    };
    if music_u.is_empty() {
        return Err("未登录网易云账号".into());
    }
    let uid: i64 = if uid.is_empty() {
        let resolved = crate::netease::resolve_uid(&music_u)?;
        {
            let conn = state.db.lock();
            db::set_setting(&conn, "netease_uid", &resolved.to_string());
        }
        resolved
    } else {
        uid.parse().map_err(|_| "账号 ID 无效".to_string())?
    };
    crate::netease::user_playlists(uid, &music_u)
}

/// 导入网易云歌单：查找/合并/创建本地播放列表并写入在线条目（播放时按权益取链接）。
/// 已导入过的（按远程歌单 id 匹配，旧数据退化同名匹配）合并进已有列表：
/// 已有条目去重跳过、本地顺序与手动加的歌不动，新歌追加到末尾。
/// 返回 (本地播放列表 id, 本次实际新增条数)
#[tauri::command]
pub async fn netease_import_playlist(
    app: AppHandle,
    remote_pid: i64, // 网易云歌单 ID
    name: String, // 歌单名（前端传入；新建/同名匹配用）
) -> Result<(i64, i64), String> {
    // 拉歌单是分页 HTTP，导入是"持 db 锁 + N 次 upsert/insert"的长循环。
    // 整段放 async 命令里会占住运行时线程，并让其它命令长时间等 db 锁。
    tauri::async_runtime::spawn_blocking(move || netease_import_playlist_blocking(app, remote_pid, name))
        .await
        .map_err(|e| format!("导入任务异常终止: {e}"))?
}

fn netease_import_playlist_blocking(
    app: AppHandle,
    remote_pid: i64,
    name: String,
) -> Result<(i64, i64), String> {
    let state = app.state::<AppState>();
    let music_u = {
        let conn = state.db.lock();
        db::get_setting(&conn, "netease_music_u").unwrap_or_default()
    };
    if music_u.is_empty() {
        return Err("未登录网易云账号".into());
    }
    let songs = crate::netease::playlist_tracks(remote_pid, &music_u)?;
    let (list_id, added) = {
        let conn = state.db.lock();
        let pid = match db::find_playlist_by_remote(
            &conn,
            "netease",
            &remote_pid.to_string(),
            &name,
        ) {
            Some(id) => id,
            None => db::create_playlist(&conn, &name)?,
        };
        db::set_playlist_remote(&conn, pid, "netease", &remote_pid.to_string());
        // 记录原始导入名：改名后重导入仍能认出（配合 remote id 兜底）
        db::set_playlist_origin(&conn, pid, &name);
        let mut added = 0i64;
        for t in &songs {
            db::upsert_online_track(
                &conn,
                "netease",
                &t.id.to_string(),
                &t.name,
                &t.artist_str(),
                &t.album_name(),
                &t.cover_url().unwrap_or_default(),
                t.duration_ms(),
                "",
                t.fee == 1,
            );
            if db::add_online_to_playlist(&conn, pid, "netease", &t.id.to_string())? {
                added += 1;
            }
        }
        (pid, added)
    };
    Ok((list_id, added))
}

#[tauri::command]
pub async fn qq_user_playlists(
    state: State<'_, AppState>,
) -> Result<Vec<crate::models::UserPlaylistMeta>, String> {
    let (musicid, musickey) = qq_credential(&state)?;
    crate::qq::user_playlists(&musicid, &musickey)
}

/// 导入 QQ 音乐歌单：远程 dissid 拉曲目 → 查找/合并/创建本地播放列表。
/// 语义与网易云版一致：已有列表去重合并、顺序不动，新歌追加末尾。
/// 返回 (本地播放列表 id, 本次实际新增条数)
#[tauri::command]
pub async fn qq_import_playlist(
    app: AppHandle,
    remote_pid: i64,
    name: String,
) -> Result<(i64, i64), String> {
    // 同 netease_import_playlist：分页 HTTP + 持锁长循环，整体移出 async 运行时
    tauri::async_runtime::spawn_blocking(move || qq_import_playlist_blocking(app, remote_pid, name))
        .await
        .map_err(|e| format!("导入任务异常终止: {e}"))?
}

fn qq_import_playlist_blocking(
    app: AppHandle,
    remote_pid: i64,
    name: String,
) -> Result<(i64, i64), String> {
    let state = app.state::<AppState>();
    let (musicid, musickey) = qq_credential(&state)?;
    let stored_euin = {
        let conn = state.db.lock();
        db::get_setting(&conn, "qq_encrypt_uin").unwrap_or_default()
    };
    let songs =
        crate::qq::playlist_tracks(remote_pid, &musicid, &musickey, &crate::qq::encrypt_uin_of(&musicid, &stored_euin))?;
    let (list_id, added) = {
        let conn = state.db.lock();
        let pid = match db::find_playlist_by_remote(
            &conn,
            "qq",
            &remote_pid.to_string(),
            &name,
        ) {
            Some(id) => id,
            None => db::create_playlist(&conn, &name)?,
        };
        db::set_playlist_remote(&conn, pid, "qq", &remote_pid.to_string());
        // 记录原始导入名：改名后重导入仍能认出（配合 remote id 兜底）
        db::set_playlist_origin(&conn, pid, &name);
        let mut added = 0i64;
        for t in &songs {
            db::upsert_online_track(
                &conn,
                "qq",
                &t.id,
                &t.name,
                &t.singer,
                &t.album,
                &format!(
                    "https://y.gtimg.cn/music/photo_new/T002R300x300M000{}.jpg",
                    t.album_mid
                ),
                t.duration_ms as i64,
                &t.media_mid,
                t.vip,
            );
            if db::add_online_to_playlist(&conn, pid, "qq", &t.id)? {
                added += 1;
            }
        }
        (pid, added)
    };
    Ok((list_id, added))
}

#[tauri::command]
pub async fn set_play_quality(state: State<'_, AppState>, quality: String) -> Result<(), String> {
    if !matches!(
        quality.as_str(),
        "standard" | "medium" | "higher" | "high" | "lossless"
    ) {
        return Err("无效的音质选项".into());
    }
    let conn = state.db.lock();
    db::set_setting(&conn, "quality", &quality);
    Ok(())
}

/// 关闭主窗口行为：tray = 最小化到托盘（默认）；exit = 直接退出应用
#[tauri::command]
pub async fn set_close_action(state: State<'_, AppState>, action: String) -> Result<(), String> {
    if !matches!(action.as_str(), "tray" | "exit") {
        return Err("无效的关闭行为".into());
    }
    let conn = state.db.lock();
    db::set_setting(&conn, "close_action", &action);
    Ok(())
}

#[tauri::command]
pub async fn play_pause(state: State<'_, AppState>) -> Result<(), String> {
    engine_clone(&state).toggle();
    Ok(())
}

/// 当前播放状态快照（含进度）。WebView 挂起恢复窗口期的事件推送可能丢失，
/// 前端恢复后主动拉取本命令做权威同步，不依赖任何固定延迟的补发。
#[tauri::command]
pub async fn get_play_state(
    state: State<'_, AppState>,
) -> Result<Option<crate::engine::PlayStateSnapshot>, String> {
    Ok(engine_clone(&state).snapshot())
}

#[tauri::command]
pub async fn pause(state: State<'_, AppState>) -> Result<(), String> {
    engine_clone(&state).pause();
    Ok(())
}

#[tauri::command]
pub async fn resume(state: State<'_, AppState>) -> Result<(), String> {
    engine_clone(&state).resume();
    Ok(())
}

#[tauri::command]
pub async fn stop(state: State<'_, AppState>) -> Result<(), String> {
    engine_clone(&state).stop();
    Ok(())
}

#[tauri::command]
pub async fn seek(state: State<'_, AppState>, ms: u64) -> Result<(), String> {
    engine_clone(&state).seek(ms)
}

#[tauri::command]
pub async fn set_volume(state: State<'_, AppState>, v: f32) -> Result<(), String> {
    engine_clone(&state).set_volume(v);
    // 立即生效，持久化交给 settings_flusher 合并写入（拖动时不逐次写库）
    *state.pending_volume.lock() = Some(format!("{v}"));
    Ok(())
}

#[tauri::command]
pub async fn set_speed(state: State<'_, AppState>, v: f32) -> Result<(), String> {
    engine_clone(&state).set_speed(v);
    *state.pending_speed.lock() = Some(format!("{v}"));
    Ok(())
}

#[tauri::command]
pub async fn set_eq(
    state: State<'_, AppState>,
    gains: Vec<f32>,
    enabled: bool,
) -> Result<(), String> {
    if gains.len() != 10 {
        return Err("均衡器需要 10 个频段的增益".into());
    }
    let mut arr = [0f32; 10];
    arr.copy_from_slice(&gains);
    engine_clone(&state).eq.set(arr, enabled);
    let conn = state.db.lock();
    db::set_setting(&conn, "eq_gains", &serde_json::to_string(&gains).unwrap());
    db::set_setting(&conn, "eq_enabled", &enabled.to_string());
    Ok(())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsPayload, String> {
    let conn = state.db.lock();
    let volume: f32 = db::get_setting(&conn, "volume")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.8);
    let speed: f32 = db::get_setting(&conn, "speed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let eq_gains: Vec<f32> = db::get_setting(&conn, "eq_gains")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| vec![0.0; 10]);
    let eq_enabled = db::get_setting(&conn, "eq_enabled")
        .map(|s| s == "true")
        .unwrap_or(false);
    let quality = db::get_setting(&conn, "quality").unwrap_or_else(|| "high".to_string());
    let cache_limit: u64 = db::get_setting(&conn, "cache_limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024 * 1024);
    let close_action =
        db::get_setting(&conn, "close_action").unwrap_or_else(|| "tray".to_string());
    let auto_update = db::get_setting(&conn, "auto_update")
        .map(|s| s != "false")
        .unwrap_or(true);
    let exclusive = db::get_setting(&conn, "exclusive")
        .map(|s| s == "true")
        .unwrap_or(false);
    Ok(SettingsPayload {
        volume,
        speed,
        eq_gains,
        eq_enabled,
        quality,
        cache_limit,
        close_action,
        auto_update,
        exclusive,
    })
}

/// 设置是否在启动时自动检查更新
#[tauri::command]
pub async fn set_auto_update(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let conn = state.db.lock();
    db::set_setting(&conn, "auto_update", if enabled { "true" } else { "false" });
    Ok(())
}

// ---------- 独占模式（WASAPI） ----------

/// 开启/关闭独占模式。
/// 开启后下一首生效；关闭则立刻把设备交还系统、从当前进度切回普通模式。
/// 返回切换后的实际状态（含回退原因与协商出的设备/采样率）。
#[tauri::command]
pub async fn set_exclusive(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<serde_json::Value, String> {
    let conn = state.db.lock();
    db::set_setting(&conn, "exclusive", if enabled { "true" } else { "false" });
    drop(conn);
    engine_clone(&state).set_exclusive(enabled)?;
    Ok(engine_clone(&state).exclusive_info())
}

/// 独占模式当前状态（界面轮询/切换后刷新用）
#[tauri::command]
pub async fn get_exclusive(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    Ok(engine_clone(&state).exclusive_info())
}

/// 试一下当前设备能不能开独占，不改变用户设置。
/// 设置页用它提前告诉用户"这台设备支不支持"，免得开了才发现白开。
///
/// 独占会话必须有采样源，这里用 0.1 秒静音桩：会话成功建立后立即终止，
/// 只为验证驱动是否允许独占与能否完成格式协商。
#[tauri::command]
pub async fn probe_exclusive(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    use std::sync::Arc;
    let eng = engine_clone(&state);
    let pref = eng.device_preference();
    let params = crate::wasapi_out::ExclusiveParams {
        device_pref: pref,
        channels: 2,
        src_rate: 48000,
        volume_bits: Arc::new(std::sync::atomic::AtomicU32::new(0.0f32.to_bits())),
        speed_bits: Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits())),
    };
    // 0.1s @48k 静音：让会话走完完整的设备初始化流程后即可判定驱动是否允许独占
    struct Silence(usize);
    impl Iterator for Silence {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if self.0 == 0 {
                return None;
            }
            self.0 -= 1;
            Some(0.0)
        }
    }
    impl rodio::Source for Silence {
        fn current_frame_len(&self) -> Option<usize> {
            Some(1)
        }
        fn sample_rate(&self) -> u32 {
            48000
        }
        fn channels(&self) -> u16 {
            2
        }
        fn total_duration(&self) -> Option<std::time::Duration> {
            None
        }
    }
    let silence = Silence(4800);
    match crate::wasapi_out::spawn_exclusive_session(silence, params) {
        Ok((ctl, rx)) => {
            // 阻塞调用挪出 async 运行时，且设上限，避免驱动无响应时卡住派发
            let opened = tauri::async_runtime::spawn_blocking(move || {
                rx.recv_timeout(std::time::Duration::from_secs(6))
            })
            .await
            .map_err(|e| e.to_string())?;
            crate::wasapi_out::wait_session_exit(&ctl, 1500);
            match opened {
                Ok(Ok(())) => Ok(json!({ "supported": true, "device": null, "rate": null, "reason": null })),
                Ok(Err(e)) => Ok(json!({ "supported": false, "device": null, "rate": null, "reason": e })),
                Err(_) => Ok(json!({ "supported": false, "device": null, "rate": null, "reason": "设备初始化超时（6 秒无响应）" })),
            }
        }
        Err(e) => Ok(json!({ "supported": false, "device": null, "rate": null, "reason": e })),
    }
}

// ---------- 其他 ----------

#[tauri::command]
pub async fn clear_cache(state: State<'_, AppState>) -> Result<u32, String> {
    Ok(engine_clone(&state).clear_cache())
}

/// 当前缓存占用（bytes）与文件数
#[tauri::command]
pub async fn cache_stats(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let (bytes, files) = engine_clone(&state).cache_usage();
    Ok(json!({ "bytes": bytes, "files": files }))
}

/// 设置缓存上限（bytes，0 = 不限制）；超限立即 LRU 清理
#[tauri::command]
pub async fn set_cache_limit(
    state: State<'_, AppState>,
    bytes: u64,
) -> Result<(), String> {
    {
        let conn = state.db.lock();
        db::set_setting(&conn, "cache_limit", &bytes.to_string());
    }
    engine_clone(&state).set_cache_limit(bytes);
    Ok(())
}

#[tauri::command]
pub async fn get_app_info(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    Ok(json!({
        "version": app.package_info().version.to_string(),
        "dataDir": state.app_data.to_string_lossy(),
    }))
}

// ---------- 封面取色（服务端，绕过 QQ 封面域无 CORS 的限制） ----------

#[tauri::command]
pub async fn extract_cover_palette(
    url: String,
) -> Result<Vec<String>, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Ok(vec![]);
    }
    // 远程封面下载 + 图片解码都是阻塞 IO（最长 10s 超时），
    // 切歌时会被高频调用，不能占住 async 运行时的工作线程
    tauri::async_runtime::spawn_blocking(move || extract_cover_palette_blocking(url))
        .await
        .map_err(|e| format!("取色任务异常终止: {e}"))?
}

fn extract_cover_palette_blocking(url: String) -> Result<Vec<String>, String> {
    let bytes = match ureq::get(&url)
        .set("User-Agent", "Mozilla/5.0")
        .timeout(std::time::Duration::from_secs(10))
        .call()
    {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader()
                .take(2 * 1024 * 1024)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            buf
        }
        Err(_) => return Ok(vec![]),
    };
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rgb = img.thumbnail(36, 36).to_rgb8();
    // 色相分桶（与前端算法一致），输出 hsl
    let mut buckets: std::collections::BTreeMap<i64, (f64, f64, f64, f64)> =
        std::collections::BTreeMap::new();
    for p in rgb.pixels() {
        let (r, g, b) = (p[0] as f64 / 255.0, p[1] as f64 / 255.0, p[2] as f64 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let lum = r * 0.3 + g * 0.6 + b * 0.1;
        if lum < 0.06 || lum > 0.97 {
            continue;
        }
        let sat = if max == 0.0 { 0.0 } else { (max - min) / max };
        let d = max - min;
        let mut hue = 0.0;
        if d != 0.0 {
            if max == r {
                hue = ((g - b) / d) % 6.0;
            } else if max == g {
                hue = (b - r) / d + 2.0;
            } else {
                hue = (r - g) / d + 4.0;
            }
            hue *= 60.0;
            if hue < 0.0 {
                hue += 360.0;
            }
        }
        let bucket = (hue / 45.0).floor() as i64;
        let w = 0.4 + sat;
        let e = buckets.entry(bucket).or_insert((0.0, 0.0, 0.0, 0.0));
        e.0 += r * w;
        e.1 += g * w;
        e.2 += b * w;
        e.3 += w;
    }
    let mut colors: Vec<(f64, f64, f64, f64)> = buckets.into_values().collect();
    colors.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    for (r, g, b, _) in colors.iter().take(4) {
        let (r, g, b) = (*r, *g, *b);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let d = max - min;
        let mut h = 0.0;
        let mut s = 0.0;
        if d != 0.0 {
            s = d / (1.0 - (2.0 * l - 1.0).abs());
            if max == r {
                h = ((g - b) / d) % 6.0;
            } else if max == g {
                h = (b - r) / d + 2.0;
            } else {
                h = (r - g) / d + 4.0;
            }
            h *= 60.0;
            if h < 0.0 {
                h += 360.0;
            }
        }
        let s2 = (s.max(0.55) * 1.1).min(1.0);
        let l_out = (l.max(0.66)).min(0.85);
        out.push(format!(
            "hsl({}, {:.0}%, {:.0}%)",
            h.round() as i64,
            s2 * 100.0,
            l_out * 100.0
        ));
    }
    Ok(out)
}

// ---------- 桌面歌词窗口 ----------

/// 打开桌面歌词窗口（透明、无边框、置顶、跳过任务栏）。
/// 已存在则仅显示与聚焦。窗口加载 desktop-lyrics.html（dev 下走 Vite 端口）。
#[tauri::command]
pub async fn desktop_lyrics_open(app: AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    if let Some(w) = app.get_webview_window("desktop-lyrics") {
        let _ = w.show();
        return Ok(());
    }
    let url = {
        // dev：Vite 服务 desktop-lyrics.html；release：dist 内多页产物
        let dev = cfg!(debug_assertions);
        let base = if dev {
            "http://localhost:1420/desktop-lyrics.html".to_string()
        } else {
            // tauri build 时 frontendDist 已包含 desktop-lyrics.html
            "desktop-lyrics.html".to_string()
        };
        base
    };
    let (w, h) = (800.0f64, 110.0f64);
    // 位置记忆：上次关闭时保存的 geometry（逻辑像素），无记录则默认主屏
    // 水平居中、垂直 82% 处（不挡任务栏）
    let default_pos = || {
        app.primary_monitor()
            .ok()
            .flatten()
            .map(|m| {
                let s = m.size();
                let sc = m.scale_factor();
                let (sw, sh) = (s.width as f64 / sc, s.height as f64 / sc);
                ((sw - w) / 2.0, sh * 0.82 - h / 2.0)
            })
            .unwrap_or((120.0, 640.0))
    };
    // "x,y,w,h" 四元组（逻辑像素）。
    // 注意：几何里存的是“歌词区高度”；实际窗口还要加上顶部 40px 的
    // 控制条预留区（前端常驻，不 hover 时全透明）——打开加、关闭减，
    // 保证歌词区始终是用户设定的高度，控制条不挤占歌词空间
    const CTRL_STRIP: f64 = 40.0;
    let saved = {
        let st = app.state::<AppState>();
        let conn = st.db.lock();
        db::get_setting(&conn, "dlyrics_geom")
    };
    let (x, y, ww, hh) = saved
        .and_then(|s| {
            let p: Vec<f64> = s.split(',').filter_map(|v| v.parse().ok()).collect();
            (p.len() == 4).then_some((p[0], p[1], p[2], p[3]))
        })
        .map(|(x, y, ww, hh)| (x, y, ww.max(320.0), hh.max(70.0)))
        .unwrap_or_else(|| {
            let (x, y) = default_pos();
            (x, y, w, h)
        });
    let builder = WebviewWindowBuilder::new(&app, "desktop-lyrics", WebviewUrl::App(url.into()))
        .title("桌面歌词")
        .inner_size(ww, hh + CTRL_STRIP)
        .position(x, (y - CTRL_STRIP).max(0.0))
        .decorations(false)
        .transparent(true)
        // WebView2 透明：alpha=0 的背景色是 Windows 下真正穿透的关键
        .background_color(tauri::utils::config::Color(0, 0, 0, 0))
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(true)
        .shadow(false);
    builder
        .build()
        .map_err(|e| format!("创建桌面歌词窗口失败: {e}"))?;
    Ok(())
}

/// 关闭桌面歌词窗口（无窗口时静默成功）；关闭前把位置尺寸存进设置表
#[tauri::command]
pub async fn desktop_lyrics_close(app: AppHandle) -> Result<(), String> {
    // 与 desktop_lyrics_open 对应：窗口高度含 40px 控制条预留区，
    // 保存几何时减掉，保证下次打开歌词区高度不变
    const CTRL_STRIP: f64 = 40.0;
    if let Some(w) = app.get_webview_window("desktop-lyrics") {
        // 几何持久化（逻辑像素四元组）
        let scale = w.scale_factor().unwrap_or(1.0);
        if let (Ok(pos), Ok(size)) = (w.outer_position(), w.inner_size()) {
            let geom = format!(
                "{},{},{},{}",
                (pos.x as f64 / scale),
                (pos.y as f64 / scale) + CTRL_STRIP,
                size.width as f64 / scale,
                ((size.height as f64 / scale) - CTRL_STRIP).max(70.0)
            );
            let st = app.state::<AppState>();
            let conn = st.db.lock();
            let _ = db::set_setting(&conn, "dlyrics_geom", &geom);
        }
        let _ = w.close();
    }
    Ok(())
}

/// 解锁桌面歌词（锁定 = set_ignore_cursor_events，穿透后窗口收不到点击，
/// 由主窗口的全局快捷键/设置开关调此命令恢复交互）
#[tauri::command]
pub async fn desktop_lyrics_unlock(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("desktop-lyrics") {
        let _ = w.set_ignore_cursor_events(false);
    }
    Ok(())
}

/// 桌面歌词窗口是否还开着。主窗口挂起期间歌词窗口可能被直接关闭
///（关闭事件丢失），恢复后查询本命令校准“词”按钮状态
#[tauri::command]
pub async fn desktop_lyrics_is_open(app: AppHandle) -> Result<bool, String> {
    Ok(app.get_webview_window("desktop-lyrics").is_some())
}

// ---------- 自动更新（GitHub Release） ----------

/// 手动检查更新：有新版本返回安装包信息，已是最新返回 null
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<updater::UpdateInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let current = app.package_info().version.to_string();
        updater::fetch_latest(&current)
    })
    .await
    .map_err(|e| format!("检查更新任务失败：{e}"))?
}

/// 前端就绪后触发一次启动自动检查（后台线程执行，结果通过
/// `update://available` 事件推送；调试构建不检查，避免开发时误装到 target 目录）
#[tauri::command]
pub async fn auto_check_update(app: AppHandle) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Ok(());
    }
    let enabled = {
        let st = app.state::<AppState>();
        let conn = st.db.lock();
        db::get_setting(&conn, "auto_update")
            .map(|s| s != "false")
            .unwrap_or(true)
    };
    if !enabled {
        return Ok(());
    }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let current = app.package_info().version.to_string();
        match updater::fetch_latest(&current) {
            Ok(Some(info)) => {
                let _ = app.emit("update://available", info);
            }
            Ok(None) => eprintln!("[updater] 已是最新版本 {current}"),
            Err(e) => eprintln!("[updater] 自动检查更新失败：{e}"),
        }
    });
    Ok(())
}

/// 下载安装包到临时目录，进度通过 `update://progress` 事件上报，返回文件路径
#[tauri::command]
pub async fn download_update(
    app: AppHandle,
    url: String,
    name: String,
    size: u64,
) -> Result<String, String> {
    // 下载可能持续数分钟：放到阻塞线程池，避免占用异步运行时
    tauri::async_runtime::spawn_blocking(move || {
        updater::download(&app, &url, &name, size)
    })
    .await
    .map_err(|e| format!("下载任务失败：{e}"))?
    .map(|p| p.to_string_lossy().to_string())
}

/// 取消进行中的下载
#[tauri::command]
pub async fn cancel_update_download() -> Result<(), String> {
    updater::cancel_download();
    Ok(())
}

/// 安装已下载的安装包并重启应用（分离助手接管后本进程自动退出）
#[tauri::command]
pub async fn install_update(app: AppHandle, path: String) -> Result<(), String> {
    updater::install_and_restart(&app, std::path::Path::new(&path))
}

/// 用系统默认浏览器打开链接（release notes 内的跳转用）
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("不支持的链接".into());
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("打开链接失败：{e}"))?;
    Ok(())
}
