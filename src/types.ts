export interface TrackMeta {
  id: number;
  path: string;
  title: string;
  artist: string;
  album: string;
  albumArtist: string;
  trackNo: number;
  disc: number;
  year: number;
  duration: number;
  format: string;
  bitrate: number;
  sampleRate: number;
  bitDepth: number;
  cover: string;
  hasLrc: boolean;
  size: number;
  mtime: number;
  liked: boolean;
  playCount: number;
  lastPlayed: number;
  /** 首次喜欢的时间（unix 秒，0 = 未喜欢过） */
  likedAt: number;
  /** 文件已不在任何监控目录（软删除：资料库隐藏，喜欢/最近播放保留记录） */
  missing: boolean;
}

export interface Folder {
  id: number;
  path: string;
}

export interface PlaylistEntryMeta {
  rowid: number;
  kind: "local" | "netease" | "qq" | "kugou" | "lx";
  trackId: number | null;
  onlineId: string | null;
  title: string;
  artist: string;
  album: string;
  cover: string;
  duration: number;
  /** QQ 在线条目的媒体 mid（播放/下载取链接需要） */
  mediaMid?: string;
  /** 在线条目是否 VIP */
  vip?: boolean;
  /** 最近播放时间（unix 秒，0 = 无记录） */
  lastPlayed?: number;
  /** 收藏时间（unix 秒，0 = 无记录） */
  likedAt?: number;
}

export interface Playlist {
  id: number;
  name: string;
  trackIds: number[];
  entries: PlaylistEntryMeta[];
  cover: string;
  createdAt: number;
  /** 来源远程歌单标识（"netease"/"qq" + 远程歌单 id；空 = 普通本地列表）。
   *  重复导入时按它合并进已有列表 */
  remoteKind?: string;
  remotePid?: string;
  /** 原始导入名：改名后仍能认出来源（重导入合并 + 界面提示用） */
  originName?: string;
}

export interface SourceItem {
  id: number;
  url: string;
  title: string;
  createdAt: number;
}

/** LX 兼容音源声明的一个平台 */
export interface LxPlatform {
  code: string;
  name: string;
  actions: string[];
  qualitys: string[];
}

/** 音源管理列表条目（设置模块） */
export interface LxSourceItem {
  id: number;
  /** script = 本地/订阅脚本；network = 接口地址 */
  kind: "script" | "network";
  name: string;
  baseUrl: string;
  origin: string;
  platforms: LxPlatform[];
  enabled: boolean;
  /** 取链协议模式："" = 标准 LX 协议；"v1" = 自定义 NestJS 端点（混淆脚本） */
  apiMode?: string;
  createdAt: number;
}

/** 添加网络音源的返回（含探测方式） */
export interface LxAddNetworkResult {
  id: number;
  created: boolean;
  kind: "script" | "network";
  name: string;
  baseUrl: string;
  via: string;
  platforms: LxPlatform[];
}

/** 排行榜 / 搜索结果里的一首歌（后端统一结构，来源可能是音源接口或内置平台） */
export interface LxSearchSong {
  id: string;
  title: string;
  artist?: string;
  album?: string;
  durationMs?: number;
  /** 平台代码：wy / tx / kg / kw …；GD 内置源时为 GD 平台码（netease/qq/kg/kw/mg） */
  platform?: string;
  /** 取链扩展上下文（酷狗 hash / QQ media_mid）；GD 源时为 url_id */
  extra?: string;
  /** 来自内置 GD音乐台源：播放走 gdstudio 匿名取链而非 LX 音源/平台登录链路 */
  viaGd?: boolean;
  /** GD 源封面 id（播放时补齐专辑图用），非 GD 结果为空 */
  picId?: string;
}

/** 排行榜搜索返回 */
export interface LxSearchResult {
  via: string;
  sourceId: number | null;
  sourceName?: string | null;
  platform: string;
  songs: LxSearchSong[];
  /** 音源自身搜索失败、已回退内置平台时的原因 */
  sourceError?: string | null;
}

export interface LyricWord {
  startMs: number;
  endMs: number;
  text: string;
}

export interface LyricLine {
  timeMs: number | null;
  text: string;
  /** 逐字时间戳（yrc/QRC/增强 LRC）；缺省时按文字长度加权推进 */
  words?: LyricWord[];
}

export interface LyricsPayload {
  synced: boolean;
  lines: LyricLine[];
}

export interface TrackInfo {
  id: number | null;
  kind: "track" | "url" | "netease" | "qq" | "kugou";
  path: string;
  title: string;
  artist: string;
  album: string;
  cover: string;
  durationMs: number;
  nid?: number | null;
  qid?: string | null;
  kgid?: string | null;
  quality?: string | null;
  /** LX 音源身份（仅来自 LX 导入音源时携带，用于回查歌词）：音源 id / 平台 / 歌曲 id */
  lxSourceId?: number | null;
  lxPlatform?: string | null;
  lxSongId?: string | null;
}

export interface QqSong {
  id: string;
  name: string;
  singer: string;
  album: string;
  albumMid: string;
  mediaMid: string;
  durationMs: number;
  vip: boolean;
}

export interface KgSong {
  /** 歌曲 hash（酷狗唯一曲目标识） */
  id: string;
  name: string;
  singer: string;
  album: string;
  durationMs: number;
  cover: string;
  vip: boolean;
}

export interface NeteaseTrack {
  id: number;
  name: string;
  ar: { name: string }[];
  al: { name: string; picUrl?: string | null };
  dt: number;
  fee: number;
}

export interface PlayState extends TrackInfo {
  playing: boolean;
  /** 后端“开播代次”（每次换曲 +1），用于区分开播与暂停/恢复 */
  seq?: number;
}

/** 播放状态快照（含进度）：WebView 挂起恢复后前端主动拉取用 */
export interface PlayStateSnapshot extends PlayState {
  pos: number;
}

export interface CurrentTrack extends TrackInfo {
  liked: boolean;
  quality?: string | null;
}

export type RepeatMode = "off" | "all" | "one";
/** 播放模式：把"循环"与"随机"归为一个四选一控件 */
export type PlayMode = "order" | "all" | "one" | "shuffle";

export type QueueItemKind =
  | "track"
  | "url"
  | "netease"
  | "qq"
  | "kugou"
  | "lx";

export type QueueItem =
  | { kind: "track" | "netease" | "url"; id: number }
  | { kind: "qq" | "kugou"; id: string }
  | { kind: "lx"; id: string };

/** LX（排行榜音源）在线曲目的播放缓存条目。
 *  id 形如 "sourceId:platform:songId"（与 online_tracks.rid 一致，可直接作缓存键） */
export interface LxQueueEntry {
  id: string;
  sourceId: number;
  platform: string;
  songId: string;
  name: string;
  artist: string;
  album: string;
  cover: string;
  extra?: string;
  durationMs: number;
}

export type ViewName =
  | "library"
  | "liked"
  | "recent"
  | "sources"
  | "ranking"
  | "netease"
  | "qq"
  | "kugou"
  | "settings"
  | "playlist"
  /** 歌手 / 专辑详情页（由 openDetailPage 压栈进入，不出现在侧边栏） */
  | "detail"
  /** 下载管理 */
  | "downloads";

export interface ScanState {
  active: boolean;
  done: number;
  total: number;
}

export interface DownloadState {
  title: string;
  pct: number;
}

export interface SettingsPayload {
  volume: number;
  speed: number;
  eqGains: number[];
  eqEnabled: boolean;
  quality: string;
  cacheLimit: number;
  /** 关闭主窗口行为：tray = 最小化到托盘（默认）；exit = 直接退出应用 */
  closeAction: "tray" | "exit";
  /** 启动时自动检查 GitHub 更新（默认开启） */
  autoUpdate: boolean;
  /** WASAPI 独占模式开关（默认关闭） */
  exclusive: boolean;
}

/** 独占模式运行状态 */
export interface ExclusiveInfo {
  /** 用户开关是否打开 */
  enabled: boolean;
  /** 是否真的在跑独占（false = 已回退普通模式） */
  active: boolean;
  /** 回退原因（active 为 true 时为 null） */
  reason: string | null;
  /** 独占生效时的设备名 */
  device: string | null;
  /** 独占生效时协商到的设备采样率 */
  rate: number | null;
}

/** 独占能力探测结果 */
export interface ExclusiveProbe {
  supported: boolean;
  device: string | null;
  rate: number | null;
  reason: string | null;
}

/** GD音乐台歌词兜底源配置 */
export interface GdStatus {
  /** 是否已开启（默认关闭） */
  enabled: boolean;
  /** 接口地址（可配置） */
  base: string;
  /** 出处标注，需在界面显示 */
  attribution: string;
}

/** 下载管理任务（后端 download_tasks 表的一行） */
export interface DownloadTask {
  id: string;
  kind: string;
  songId: string;
  mediaMid: string;
  title: string;
  artist: string;
  album: string;
  cover: string;
  /** 字节；服务端未给 content-length 时为 0 */
  size: number;
  received: number;
  status: "queued" | "downloading" | "done" | "failed";
  error: string;
  filePath: string;
  createdAt: number;
  finishedAt: number;
}

/** GitHub 最新 release 的可安装更新信息 */
export interface UpdateInfo {
  /** 最新版本号（不含 v 前缀） */
  version: string;
  /** release notes（markdown 原文） */
  notes: string;
  assetName: string;
  assetUrl: string;
  assetSize: number;
  publishedAt: string;
}

export interface UserPlaylistMeta {
  id: number;
  name: string;
  trackCount: number;
}

export interface Toast {
  id: number;
  msg: string;
  type: "info" | "error" | "success";
}
