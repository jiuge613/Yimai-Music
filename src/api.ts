import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import type {
  ExclusiveInfo,
  ExclusiveProbe,
  Folder,
  GdStatus,
  LyricsPayload,
  NeteaseTrack,
  Playlist,
  QqSong,
  UpdateInfo,
  UserPlaylistMeta,
  PlayState,
  PlayStateSnapshot,
  ScanState,
  SettingsPayload,
  SourceItem,
  TrackMeta,
  LxAddNetworkResult,
  LxSourceItem,
  LxSearchResult,
} from "./types";

export const api = {
  listTracks: () => invoke<TrackMeta[]>("list_tracks"),
  listFolders: () => invoke<Folder[]>("list_folders"),
  addFolder: (path: string) => invoke<void>("add_folder", { path }),
  removeFolder: (id: number) => invoke<void>("remove_folder", { id }),
  rescan: () => invoke<void>("rescan"),
  openFolder: (path: string) => invoke<void>("open_folder", { path }),
  listOutputDevices: () =>
    invoke<{
      devices: { name: string; isDefault: boolean }[];
      current: string;
      preference: string | null;
    }>("list_output_devices"),
  setOutputDevice: (name: string | null) =>
    invoke<void>("set_output_device", { name }),
  dropPaths: (paths: string[]) => invoke<number>("drop_paths", { paths }),
  getLyrics: (trackId: number) => invoke<LyricsPayload>("get_lyrics", { trackId }),
  /** 备用歌词源（兜底）：按「歌名+歌手」搜网易云取最佳匹配歌词，主源无歌词时调用 */
  backupLyric: (title: string, artist: string) =>
    invoke<LyricsPayload>("backup_lyric", { title, artist }),
  // ---------- GD音乐台 歌词兜底（默认关闭，需用户在设置里自行确认 CC BY-NC 条款） ----------
  gdLyric: (title: string, artist: string) =>
    invoke<LyricsPayload>("gd_lyric", { title, artist }),
  gdStatus: () => invoke<GdStatus>("gd_status"),
  setGdFallback: (enabled: boolean, base?: string) =>
    invoke<GdStatus>("set_gd_fallback", { enabled, base: base ?? null }),
  likeTrack: (id: number, liked: boolean) =>
    invoke<void>("like_track", { id, liked }),
  /** 移除本地歌曲记录（仅标记，不删磁盘文件） */
  removeTrack: (id: number) => invoke<void>("remove_track", { id }),
  listPlaylists: () => invoke<Playlist[]>("list_playlists"),
  createPlaylist: (name: string) => invoke<number>("create_playlist", { name }),
  deletePlaylist: (id: number) => invoke<void>("delete_playlist", { id }),
  renamePlaylist: (id: number, name: string) =>
    invoke<void>("rename_playlist", { id, name }),
  addToPlaylist: (playlistId: number, trackId: number) =>
    invoke<void>("add_to_playlist", { playlistId, trackId }),
  removeFromPlaylist: (playlistId: number, trackId: number) =>
    invoke<void>("remove_from_playlist", { playlistId, trackId }),
  listSources: () => invoke<SourceItem[]>("list_sources"),
  addSource: (url: string, title?: string) =>
    invoke<number>("add_source", { url, title: title ?? "" }),
  deleteSource: (id: number) => invoke<void>("delete_source", { id }),
  // ---------- 音源管理（LX 兼容脚本 / 网络接口音源） ----------
  lxListSources: () => invoke<LxSourceItem[]>("lx_list_sources"),
  lxAddScriptSource: (
    content: string,
    baseUrl?: string,
    apiMode?: string
  ) =>
    invoke<{
      id: number;
      created: boolean;
      name: string;
      baseUrl: string;
      apiMode: string;
    }>("lx_add_script_source", {
      content,
      baseUrl: baseUrl ?? null,
      apiMode: apiMode ?? null,
    }),
  lxAddNetworkSource: (url: string) =>
    invoke<LxAddNetworkResult>("lx_add_network_source", { url }),
  lxSetSourceEnabled: (id: number, enabled: boolean) =>
    invoke<void>("lx_set_source_enabled", { id, enabled }),
  lxDeleteSource: (id: number) => invoke<void>("lx_delete_source", { id }),
  lxReadScriptFile: (path: string) => invoke<string>("lx_read_script_file", { path }),
  lxResolveUrl: (args: {
    sourceId: number;
    platform: string;
    songId: string;
    quality?: string;
    extra?: string;
  }) => invoke<string>("lx_resolve_url", args),
  /** 排行榜搜索：优先音源 search.php，失败回退内置平台（网易云/QQ/酷狗） */
  lxSearch: (args: {
    sourceId?: number | null;
    platform?: string;
    keyword: string;
    limit?: number;
  }) => invoke<LxSearchResult>("lx_search", {
    sourceId: args.sourceId ?? null,
    platform: args.platform ?? "",
    keyword: args.keyword,
    limit: args.limit ?? 30,
  }),
  /** 排行榜曲目播放：用指定音源取链后走统一在线播放链路 */
  lxPlaySong: (req: {
    sourceId: number;
    platform: string;
    songId: string;
    title?: string;
    artist?: string;
    album?: string;
    cover?: string;
    durationMs?: number;
    quality?: string;
    extra?: string;
  }) => invoke<void>("lx_play_song", req),
  /** LX 音源歌词：凭播放时携带的音源身份回查 lyric.php */
  lxLyric: (sourceId: number, platform: string, songId: string) =>
    invoke<LyricsPayload>("lx_lyric", { sourceId, platform, songId }),
  /** 下载 LX 音源曲目（凭播放时携带的音源身份取链后下载到本地资料库） */
  lxDownload: (req: {
    sourceId: number;
    platform: string;
    songId: string;
    title: string;
    artist: string;
    album: string;
    cover: string;
    durationMs: number;
    extra?: string;
  }) => invoke<string>("lx_download", { req }),
  neteaseSearch: (keyword: string, offset: number) =>
    invoke<{ total: number; songs: NeteaseTrack[] }>("netease_search", {
      keyword,
      offset,
    }),
  neteasePlay: (track: {
    id: number;
    title: string;
    artist: string;
    album: string;
    cover: string;
    durationMs: number;
  }) => invoke<void>("netease_play", { track }),
  neteaseStatus: () => invoke<{ loggedIn: boolean; nickname: string }>("netease_status"),
  neteaseQrCreate: () => invoke<{ key: string; qr: string }>("netease_qr_create"),
  neteaseQrCheck: (key: string) =>
    invoke<{ status: string; nickname?: string }>("netease_qr_check", { key }),
  neteaseLyric: (id: number) => invoke<LyricsPayload>("netease_lyric", { id }),
  qqSearch: (keyword: string, page: number) =>
    invoke<{ songs: QqSong[] }>("qq_search", { keyword, page }),
  qqPlay: (track: {
    songmid: string;
    title: string;
    artist: string;
    album: string;
    albumMid: string;
    mediaMid: string;
    durationMs: number;
    vip: boolean;
  }) => invoke<void>("qq_play", { track }),
  qqLyric: (songmid: string) => invoke<LyricsPayload>("qq_lyric", { songmid }),
  kugouSearch: (keyword: string, page: number) =>
    invoke<{ songs: import("./types").KgSong[] }>("kugou_search", { keyword, page }),
  kugouPlay: (track: {
    hash: string;
    title: string;
    artist: string;
    album: string;
    cover: string;
    durationMs: number;
    vip: boolean;
  }) => invoke<void>("kugou_play", { track }),
  kugouLyric: (hash: string) => invoke<LyricsPayload>("kugou_lyric", { hash }),
  qqQrCreate: () => invoke<{ qrsig: string; qr: string }>("qq_qr_create"),
  qqQrCheck: (qrsig: string) =>
    invoke<{ status: string; nickname?: string }>("qq_qr_check", { qrsig }),
  qqStatus: () => invoke<{ loggedIn: boolean; nickname: string }>("qq_status"),
  qqLogout: () => invoke<void>("qq_logout"),
  neteaseLikeList: () => invoke<number[]>("netease_like_list"),
  neteaseLike: (id: number, like: boolean) =>
    invoke<void>("netease_like", { id, like }),
  neteaseLogout: () => invoke<void>("netease_logout"),
  likeOnline: (req: {
    kind: string;
    rid: string;
    title: string;
    artist: string;
    album: string;
    cover: string;
    durationMs: number;
    mediaMid: string;
    vip: boolean;
    like: boolean;
  }) => invoke<void>("like_online", req),
  downloadOnline: (req: {
    kind: string;
    id: string;
    title: string;
    artist: string;
    album: string;
    coverUrl: string;
    durationMs: number;
    mediaMid: string;
  }) => invoke<string>("download_online", { req }),
  likedOnlineList: () =>
    invoke<import("./types").PlaylistEntryMeta[]>("liked_online_list"),
  recentOnlineList: () =>
    invoke<import("./types").PlaylistEntryMeta[]>("recent_online_list"),
  saveDirGet: () =>
    invoke<{ dir: string; default: string }>("save_dir_get"),
  saveDirSet: (dir: string) => invoke<void>("save_dir_set", { dir }),
  addOnlineToPlaylist: (req: {
    playlistId: number;
    kind: string;
    rid: string;
    title: string;
    artist: string;
    album: string;
    cover: string;
    durationMs: number;
    mediaMid: string;
    vip: boolean;
  }) => invoke<void>("add_online_to_playlist", req),
  removePlaylistEntry: (rowid: number) =>
    invoke<void>("remove_playlist_entry", { rowid }),
  saveManualOrder: (list: string, keys: string[]) =>
    invoke<void>("save_manual_order", { list, keys }),
  getManualOrder: (list: string) =>
    invoke<Record<string, number>>("get_manual_order", { list }),
  reorderPlaylist: (playlistId: number, rowids: number[]) =>
    invoke<void>("reorder_playlist", { playlistId, rowids }),
  reorderPlaylists: (ids: number[]) =>
    invoke<void>("reorder_playlists", { ids }),
  neteaseUserPlaylists: () =>
    invoke<UserPlaylistMeta[]>("netease_user_playlists"),
  neteaseImportPlaylist: (remotePid: number, name: string) =>
    invoke<[number, number]>("netease_import_playlist", { remotePid, name }),
  qqUserPlaylists: () =>
    invoke<import("./types").UserPlaylistMeta[]>("qq_user_playlists"),
  qqImportPlaylist: (remotePid: number, name: string) =>
    invoke<[number, number]>("qq_import_playlist", { remotePid, name }),
  setPlayQuality: (quality: string) => invoke<void>("set_play_quality", { quality }),
  setCloseAction: (action: string) => invoke<void>("set_close_action", { action }),
  setAutoUpdate: (enabled: boolean) => invoke<void>("set_auto_update", { enabled }),
  // ---------- 独占模式（WASAPI 直连声卡） ----------
  /** 开启下一首生效；关闭则立即交还设备并切回普通模式 */
  setExclusive: (enabled: boolean) => invoke<ExclusiveInfo>("set_exclusive", { enabled }),
  getExclusive: () => invoke<ExclusiveInfo>("get_exclusive"),
  /** 试探当前设备能否独占（不改变用户设置） */
  probeExclusive: () => invoke<ExclusiveProbe>("probe_exclusive"),
  // ---------- 自动更新（GitHub Release） ----------
  /** 前端就绪后触发一次启动自动检查（结果通过 update://available 事件推送） */
  autoCheckUpdate: () => invoke<void>("auto_check_update"),
  /** 手动检查：有更新返回信息，已是最新返回 null */
  checkUpdate: () => invoke<UpdateInfo | null>("check_update"),
  /** 下载安装包到临时目录，进度走 update://progress 事件，返回文件路径 */
  downloadUpdate: (req: { url: string; name: string; size: number }) =>
    invoke<string>("download_update", req),
  cancelUpdateDownload: () => invoke<void>("cancel_update_download"),
  /** 安装并重启应用（安装位置不变，安装完成后自动重启） */
  installUpdate: (path: string) => invoke<void>("install_update", { path }),
  /** 用系统浏览器打开链接（release notes 内跳转用） */
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  extractCoverPalette: (url: string) => invoke<string[]>("extract_cover_palette", { url }),
  playTrack: (id: number) => invoke<void>("play_track", { id }),
  playSource: (id: number) => invoke<void>("play_source", { id }),
  playPause: () => invoke<void>("play_pause"),
  getPlayState: () => invoke<PlayStateSnapshot | null>("get_play_state"),
  pause: () => invoke<void>("pause"),
  resume: () => invoke<void>("resume"),
  stop: () => invoke<void>("stop"),
  seek: (ms: number) => invoke<void>("seek", { ms }),
  setVolume: (v: number) => invoke<void>("set_volume", { v }),
  setSpeed: (v: number) => invoke<void>("set_speed", { v }),
  setEq: (gains: number[], enabled: boolean) =>
    invoke<void>("set_eq", { gains, enabled }),
  getSettings: () => invoke<SettingsPayload>("get_settings"),
  /** 最近一次扫描进度快照（WebView 挂起恢复后补发用） */
  getScanState: () => invoke<ScanState>("get_scan_state"),
  clearCache: () => invoke<number>("clear_cache"),
  cacheStats: () =>
    invoke<{ bytes: number; files: number }>("cache_stats"),
  setCacheLimit: (bytes: number) =>
    invoke<void>("set_cache_limit", { bytes }),
  getAppInfo: () => invoke<{ version: string; dataDir: string }>("get_app_info"),
  desktopLyricsOpen: () => invoke<void>("desktop_lyrics_open"),
  desktopLyricsClose: () => invoke<void>("desktop_lyrics_close"),
  desktopLyricsUnlock: () => invoke<void>("desktop_lyrics_unlock"),
  desktopLyricsIsOpen: () => invoke<boolean>("desktop_lyrics_is_open"),
};

export { convertFileSrc };

export function coverSrc(path: string): string {
  if (!path) return "";
  // http(s) 封面直接用原 URL；本地文件路径才转 asset 协议
  if (path.startsWith("http://") || path.startsWith("https://")) return path;
  return convertFileSrc(path);
}

export type ListenerUnbind = () => void;

export async function listenEvent<T>(
  event: string,
  handler: (payload: T) => void
): Promise<ListenerUnbind> {
  const { listen } = await import("@tauri-apps/api/event");
  return listen<T>(event, (e) => handler(e.payload));
}

export type {
  Folder,
  LyricsPayload,
  Playlist,
  PlayState,
  ScanState,
  SettingsPayload,
  SourceItem,
  TrackMeta,
  UpdateInfo,
};
