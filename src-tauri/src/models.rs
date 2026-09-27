use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrackMeta {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: i64,
    pub disc: i64,
    pub year: i64,
    pub duration: f64,
    pub format: String,
    pub bitrate: i64,
    pub sample_rate: i64,
    pub bit_depth: i64,
    pub cover: String,
    pub has_lrc: bool,
    pub size: i64,
    pub mtime: i64,
    pub liked: bool,
    pub play_count: i64,
    pub last_played: i64,
    /// 首次喜欢的时间（unix 秒，0 = 未喜欢过；“我喜欢”排序用）
    pub liked_at: i64,
    /// 文件已不在任何监控目录下（软删除：保留记录供“我喜欢/最近播放”，资料库隐藏）
    pub missing: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: i64,
    pub path: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntryMeta {
    pub rowid: i64,
    /// local | netease | qq
    pub kind: String,
    pub track_id: Option<i64>,
    pub online_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
    pub duration: f64,
    /// 在线条目（QQ）的媒体 mid，播放/下载取链接时需要
    #[serde(default)]
    pub media_mid: String,
    /// 在线条目是否 VIP 曲目（前端显示 VIP 角标）
    #[serde(default)]
    pub vip: bool,
    /// 最近播放时间（unix 秒，0 = 无记录；“最近播放”合并排序用）
    #[serde(default)]
    pub last_played: i64,
    /// 收藏时间（unix 秒，0 = 无记录；“我喜欢”合并排序用）
    #[serde(default)]
    pub liked_at: i64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub track_ids: Vec<i64>,
    pub entries: Vec<PlaylistEntryMeta>,
    /// 第一首歌的封面（在线条目为 URL，本地为文件路径）
    pub cover: String,
    pub created_at: i64,
    /// 来源远程歌单标识（netease/qq + 远程歌单 id；空 = 普通本地列表）。
    /// 重复导入时按它合并进已有列表
    #[serde(default)]
    pub remote_kind: String,
    #[serde(default)]
    pub remote_pid: String,
    /// 原始导入名（导入时的远程歌单名）：改名后仍能认出来源，
    /// 界面提示 + 重导入按名兜底匹配用
    #[serde(default)]
    pub origin_name: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SourceItem {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub created_at: i64,
}

/// LX 兼容音源声明的一个平台（脚本 sources 块或 platforms.php 的条目）
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LxPlatform {
    /// 平台代码：wy / tx / kw / kg / mg / joox …
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub qualitys: Vec<String>,
}

/// 音源管理列表条目（lx_sources 表）
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LxSourceItem {
    pub id: i64,
    /// script = 本地/订阅音源脚本；network = 直接登记的接口地址
    pub kind: String,
    pub name: String,
    /// 取链接口基址（无尾斜杠），所有请求发往 {base}/url.php 等
    pub base_url: String,
    /// script 类型保留原始脚本文本（重新解析 / 查看用）；network 为空
    #[serde(default)]
    pub origin: String,
    /// 平台与音质契约（JSON 数组）
    #[serde(default)]
    pub platforms: Vec<LxPlatform>,
    pub enabled: bool,
    /// 取链协议模式："" = 标准 LX 协议（{base}/url.php）；"v1" = 自定义
    /// NestJS 端点（POST {base}/v1/music/resolve-url）。混淆脚本（运行时解码基址）
    /// 经前端执行取链后回填此字段，避免后端静态解析失败。
    #[serde(default)]
    pub api_mode: String,
    pub created_at: i64,
}

/// 排行榜 / 搜索结果中的一首歌（统一结构）
///
/// 来源可能是音源接口（search.php）或内置平台（网易云 / QQ / 酷狗），
/// 前端只认这一份结构；`platform` 决定取链时走哪个平台代码。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LxSearchSong {
    /// 曲目 ID（取链用；各平台语义不同：网易云=数字 id、QQ=songmid、酷狗=hash）
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// 平台代码：wy / tx / kg / kw / mg …
    #[serde(default)]
    pub platform: String,
    /// 取链扩展上下文（酷狗 hash / QQ media_mid 等），原样透传给 url.php
    #[serde(default)]
    pub extra: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub time_ms: Option<u64>,
    pub text: String,
    /// 逐字时间戳（yrc/增强 LRC）；缺省时前端按文字长度加权推进
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<Word>>,
}

/// 一个字（或词）的起止时间
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Word {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LyricsPayload {
    pub synced: bool,
    pub lines: Vec<LyricLine>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPayload {
    pub volume: f32,
    pub speed: f32,
    pub eq_gains: Vec<f32>,
    pub eq_enabled: bool,
    /// standard(128k) | medium(192k) | higher(256k) | high(320k) | lossless(FLAC/WAV)
    pub quality: String,
    /// 音源缓存上限（字节），0 = 不限制
    pub cache_limit: u64,
    /// 关闭主窗口行为：tray（默认，隐藏到托盘）| exit（退出应用）
    pub close_action: String,
    /// 启动时自动检查 GitHub 更新（默认开启）
    pub auto_update: bool,
    /// WASAPI 独占模式开关（默认关闭）
    pub exclusive: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UserPlaylistMeta {
    pub id: i64,
    pub name: String,
    pub track_count: i64,
}
