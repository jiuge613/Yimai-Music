use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::models::{Folder, LxSourceItem, Playlist, SourceItem, TrackMeta};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS folders (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT UNIQUE NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  album TEXT NOT NULL DEFAULT '',
  album_artist TEXT NOT NULL DEFAULT '',
  track_no INTEGER NOT NULL DEFAULT 0,
  disc INTEGER NOT NULL DEFAULT 0,
  year INTEGER NOT NULL DEFAULT 0,
  duration REAL NOT NULL DEFAULT 0,
  format TEXT NOT NULL DEFAULT '',
  bitrate INTEGER NOT NULL DEFAULT 0,
  sample_rate INTEGER NOT NULL DEFAULT 0,
  bit_depth INTEGER NOT NULL DEFAULT 0,
  cover TEXT NOT NULL DEFAULT '',
  lrc_path TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0,
  mtime INTEGER NOT NULL DEFAULT 0,
  added_at INTEGER NOT NULL DEFAULT 0,
  missing INTEGER NOT NULL DEFAULT 0,
  removed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS playlists (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT 0,
  remote_kind TEXT NOT NULL DEFAULT '',
  remote_pid TEXT NOT NULL DEFAULT '',
  origin_name TEXT NOT NULL DEFAULT '',
  sort_pos INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
  playlist_id INTEGER NOT NULL,
  track_id INTEGER NOT NULL,
  position INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL DEFAULT 'local',
  online_id TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (playlist_id, kind, online_id, track_id)
);
CREATE TABLE IF NOT EXISTS sources (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  url TEXT UNIQUE NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0
);
-- 音源管理（LX 兼容脚本 / 网络接口音源）。platforms 存 JSON 数组
CREATE TABLE IF NOT EXISTS lx_sources (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL DEFAULT 'network',
  name TEXT NOT NULL DEFAULT '',
  base_url TEXT NOT NULL,
  origin TEXT NOT NULL DEFAULT '',
  platforms TEXT NOT NULL DEFAULT '[]',
  enabled INTEGER NOT NULL DEFAULT 1,
  api_mode TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS stats (
  track_id INTEGER PRIMARY KEY,
  play_count INTEGER NOT NULL DEFAULT 0,
  last_played INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS liked (
  track_id INTEGER PRIMARY KEY,
  liked_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS online_tracks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  rid TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  album TEXT NOT NULL DEFAULT '',
  cover TEXT NOT NULL DEFAULT '',
  duration_ms INTEGER NOT NULL DEFAULT 0,
  media_mid TEXT NOT NULL DEFAULT '',
  vip INTEGER NOT NULL DEFAULT 0,
  downloaded INTEGER NOT NULL DEFAULT 0,
  last_played INTEGER NOT NULL DEFAULT 0,
  play_count INTEGER NOT NULL DEFAULT 0,
  UNIQUE(kind, rid)
);
CREATE TABLE IF NOT EXISTS liked_online (
  rowid INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  rid TEXT NOT NULL,
  liked_at INTEGER NOT NULL DEFAULT 0,
  UNIQUE(kind, rid)
);
-- “资料库/我喜欢”的手动排序（列表 key + 行 key → 序号）；
-- 播放列表不用它（playlist_tracks 自带 position 列）
CREATE TABLE IF NOT EXISTS manual_order (
  list TEXT NOT NULL,
  row_key TEXT NOT NULL,
  pos INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (list, row_key)
);
-- 下载管理任务表：把「点了下载」变成可查看、可重试、可删除的任务。
-- id 形如 "netease:123"，同一首歌重复下载复用同一行（幂等，不会堆重复记录）。
CREATE TABLE IF NOT EXISTS download_tasks (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  song_id TEXT NOT NULL,
  media_mid TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  artist TEXT NOT NULL DEFAULT '',
  album TEXT NOT NULL DEFAULT '',
  cover TEXT NOT NULL DEFAULT '',
  size INTEGER NOT NULL DEFAULT 0,
  received INTEGER NOT NULL DEFAULT 0,
  -- queued | downloading | done | failed
  status TEXT NOT NULL DEFAULT 'queued',
  error TEXT NOT NULL DEFAULT '',
  file_path TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0,
  finished_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_download_status ON download_tasks(status, created_at DESC);
"#;

/// 查询用索引。放在 migrate() 尾部而不是 SCHEMA 里：
/// - tracks.removed / online_tracks.last_played 是 migrate 渐进补出来的列，
///   SCHEMA 先于 migrate 执行，在旧库上建索引会 "no such column" 直接失败；
/// - playlist_tracks 会被主键重建整表替换，索引必须建在重建之后。
/// playlist_tracks(playlist_id, kind, online_id, track_id) 的复合主键只能用到首列，
/// 所以单列 playlist_id 查询和 remove_from_playlist 的
/// DELETE ... WHERE playlist_id=? AND track_id=? 都需要额外索引兜底。
const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_tracks_removed ON tracks(removed)",
    "CREATE INDEX IF NOT EXISTS idx_online_tracks_recent
       ON online_tracks(last_played DESC) WHERE last_played > 0",
    "CREATE INDEX IF NOT EXISTS idx_playlist_tracks_pid
       ON playlist_tracks(playlist_id, position)",
    "CREATE INDEX IF NOT EXISTS idx_liked_online_kind ON liked_online(kind)",
];

// playlist_tracks 的 kind/online_id 列为渐进迁移（旧库自动补列）
pub fn migrate(conn: &Connection) {
    // 逐条补列：列已存在是预期情况（“duplicate column”），必须跳过继续；
    // 若放进一个 execute_batch，第一条失败会中止整批，后续列永远补不上
    // （线上曾因此停留在旧 schema：playlist_entries/recent_online_list
    //  查询 last_played/missing 静默失败，歌单条目与最近播放全空）
    add_column_if_missing(
        conn,
        "online_tracks",
        "downloaded",
        "ALTER TABLE online_tracks ADD COLUMN downloaded INTEGER NOT NULL DEFAULT 0",
    );
    add_column_if_missing(
        conn,
        "tracks",
        "missing",
        "ALTER TABLE tracks ADD COLUMN missing INTEGER NOT NULL DEFAULT 0",
    );
    add_column_if_missing(
        conn,
        "online_tracks",
        "last_played",
        "ALTER TABLE online_tracks ADD COLUMN last_played INTEGER NOT NULL DEFAULT 0",
    );
    add_column_if_missing(
        conn,
        "online_tracks",
        "play_count",
        "ALTER TABLE online_tracks ADD COLUMN play_count INTEGER NOT NULL DEFAULT 0",
    );
    add_column_if_missing(
        conn,
        "liked",
        "liked_at",
        "ALTER TABLE liked ADD COLUMN liked_at INTEGER NOT NULL DEFAULT 0",
    );
    // playlists 的远程歌单标识（netease/qq 的歌单 id）：重复导入时按它合并进
    // 已有列表，而不是再建一个同名列表
    add_column_if_missing(
        conn,
        "playlists",
        "remote_kind",
        "ALTER TABLE playlists ADD COLUMN remote_kind TEXT NOT NULL DEFAULT ''",
    );
    add_column_if_missing(
        conn,
        "playlists",
        "remote_pid",
        "ALTER TABLE playlists ADD COLUMN remote_pid TEXT NOT NULL DEFAULT ''",
    );
    // 播放列表的原始导入名：改名后仍能知道它来自哪个远程歌单（重导入合并、
    // 界面提示用）
    add_column_if_missing(
        conn,
        "playlists",
        "origin_name",
        "ALTER TABLE playlists ADD COLUMN origin_name TEXT NOT NULL DEFAULT ''",
    );
    // 播放列表手动排序位（侧边栏长按拖动调序）：旧行回填为 id，保持原顺序
    add_column_if_missing(
        conn,
        "playlists",
        "sort_pos",
        "ALTER TABLE playlists ADD COLUMN sort_pos INTEGER NOT NULL DEFAULT 0",
    );
    // 音源取链协议模式："" = 标准 LX 协议；"v1" = 自定义 NestJS 端点。
    // 混淆脚本（运行时解码基址）经前端执行取链后回填，旧库默认空（标准协议）。
    add_column_if_missing(
        conn,
        "lx_sources",
        "api_mode",
        "ALTER TABLE lx_sources ADD COLUMN api_mode TEXT NOT NULL DEFAULT ''",
    );
    // 用户主动移除的本地歌曲：扫描/导入跳过、资料库不显示（区别于文件缺失的 missing）
    add_column_if_missing(
        conn,
        "tracks",
        "removed",
        "ALTER TABLE tracks ADD COLUMN removed INTEGER NOT NULL DEFAULT 0",
    );
    let _ = conn.execute(
        "UPDATE playlists SET sort_pos = id WHERE sort_pos = 0",
        [],
    );
    // playlist_tracks 旧主键 (playlist_id, track_id) 会吞掉同列表的多个在线条目
    // （track_id 恒为 0），检测旧结构并重建为 (playlist_id, kind, online_id, track_id)
    let old_pk: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='playlist_tracks'",
            [],
            |r| r.get(0),
        )
        .ok();
    let needs_rebuild = old_pk
        .as_deref()
        .map(|sql| sql.contains("PRIMARY KEY (playlist_id, track_id)"))
        .unwrap_or(false);
    if needs_rebuild {
        // 逐句执行并显式控制事务：原先把整段塞进 execute_batch 且忽略返回值，
        // 一旦 DROP 之后某句失败，事务悬着、旧表已丢，歌单条目会静默消失。
        // 这里改成 BEGIN → 逐句 → COMMIT，任何一步失败都 ROLLBACK 回旧表。
        const REBUILD: &[&str] = &[
            "CREATE TABLE playlist_tracks_new (
               playlist_id INTEGER NOT NULL,
               track_id INTEGER NOT NULL,
               position INTEGER NOT NULL DEFAULT 0,
               kind TEXT NOT NULL DEFAULT 'local',
               online_id TEXT NOT NULL DEFAULT '',
               PRIMARY KEY (playlist_id, kind, online_id, track_id)
             )",
            "INSERT OR IGNORE INTO playlist_tracks_new
               (playlist_id, track_id, position, kind, online_id)
             SELECT playlist_id, track_id, position,
               COALESCE(NULLIF(kind, ''), 'local'), COALESCE(online_id, '')
             FROM playlist_tracks",
            "DROP TABLE playlist_tracks",
            "ALTER TABLE playlist_tracks_new RENAME TO playlist_tracks",
        ];
        if let Err(e) = conn.execute_batch("BEGIN") {
            eprintln!("[db] playlist_tracks 重建失败（BEGIN）: {e}");
            return;
        }
        let mut failed = None;
        for stmt in REBUILD {
            if let Err(e) = conn.execute_batch(stmt) {
                failed = Some((*stmt, e));
                break;
            }
        }
        match failed {
            Some((stmt, e)) => {
                let _ = conn.execute_batch("ROLLBACK");
                eprintln!("[db] playlist_tracks 重建失败，已回滚: {e}\n  语句: {stmt}");
            }
            None => {
                if let Err(e) = conn.execute_batch("COMMIT") {
                    let _ = conn.execute_batch("ROLLBACK");
                    eprintln!("[db] playlist_tracks 重建提交失败，已回滚: {e}");
                }
            }
        }
    }

    // 索引最后建：既在补列之后，也在 playlist_tracks 整表重建之后。
    // 单条失败只记日志不中断——索引缺失只是查询变慢，不该让整个库打不开。
    for stmt in INDEXES {
        if let Err(e) = conn.execute_batch(stmt) {
            eprintln!("[db] 建索引失败（已跳过）: {e}\n  语句: {stmt}");
        }
    }
}

/// ALTER TABLE ... ADD COLUMN，列已存在时静默跳过（幂等迁移）
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, alter: &str) {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !exists {
        if let Err(e) = conn.execute_batch(alter) {
            eprintln!("[db] 迁移补列失败 {table}.{column}: {e}");
        }
    }
}

pub fn init(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    migrate(&conn);
    Ok(conn)
}

// ---------- settings ----------

pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value],
    );
}

// ---------- folders ----------

pub fn list_folders(conn: &Connection) -> Vec<Folder> {
    let mut stmt = match conn.prepare("SELECT id, path FROM folders ORDER BY id") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |r| {
        Ok(Folder { id: r.get(0)?, path: r.get(1)? })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub fn add_folder(conn: &Connection, path: &str) -> Result<i64, String> {
    conn.execute("INSERT OR IGNORE INTO folders(path) VALUES(?1)", params![path])
        .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn remove_folder(conn: &Connection, id: i64) {
    let path: Option<String> = conn
        .query_row("SELECT path FROM folders WHERE id = ?1", params![id], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    if let Some(p) = path {
        // 软删除该文件夹下的全部曲目（missing=1）：
        // 记录保留（喜欢/最近播放仍显示），文件夹重新添加后扫描复活
        let sep = format!("{p}\\");
        let _ = conn.execute(
            "UPDATE tracks SET missing = 1
             WHERE path = ?1 OR substr(path, 1, ?2) = ?3",
            params![p, sep.len() as i64, sep],
        );
    }
    let _ = conn.execute("DELETE FROM folders WHERE id = ?1", params![id]);
}

// ---------- tracks ----------

pub struct NewTrack {
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
    pub lrc_path: String,
    pub size: i64,
    pub mtime: i64,
}

pub fn upsert_track(conn: &Connection, t: &NewTrack) {
    let _ = conn.execute(
        r#"INSERT INTO tracks(path, title, artist, album, album_artist, track_no, disc, year,
             duration, format, bitrate, sample_rate, bit_depth, cover, lrc_path, size, mtime, added_at)
           VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
           ON CONFLICT(path) DO UPDATE SET
             title=?2, artist=?3, album=?4, album_artist=?5, track_no=?6, disc=?7, year=?8,
             duration=?9, format=?10, bitrate=?11, sample_rate=?12, bit_depth=?13,
             cover=?14, lrc_path=?15, size=?16, mtime=?17, removed=0, missing=0"#,
        params![
            t.path, t.title, t.artist, t.album, t.album_artist, t.track_no, t.disc, t.year,
            t.duration, t.format, t.bitrate, t.sample_rate, t.bit_depth, t.cover, t.lrc_path,
            t.size, t.mtime, now_secs(),
        ],
    );
}

pub fn track_paths(conn: &Connection) -> Vec<(String, i64, i64)> {
    let mut stmt = match conn.prepare("SELECT path, mtime, size FROM tracks") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// 被用户主动移除的曲目路径（扫描/导入时跳过，避免自动复活）
pub fn removed_track_paths(conn: &Connection) -> std::collections::HashSet<String> {
    let mut stmt = match conn.prepare("SELECT path FROM tracks WHERE removed = 1") {
        Ok(s) => s,
        Err(_) => return std::collections::HashSet::new(),
    };
    stmt.query_map([], |r| r.get(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// 移除指定本地歌曲记录（仅标记，不动磁盘文件）：
/// 资料库不再显示，扫描时不再重新导入；磁盘文件保留，歌单引用仍可播放。
pub fn mark_track_removed(conn: &Connection, id: i64) -> bool {
    let n = conn
        .execute("UPDATE tracks SET removed = 1 WHERE id = ?1", params![id])
        .unwrap_or(0);
    n > 0
}

/// 文件已不存在时软删除（missing=1）：记录保留（含喜欢/播放统计/歌单引用），
/// 资料库隐藏；文件重新加入后 upsert 自动复活
pub fn delete_missing(conn: &Connection, seen: &HashSet<String>, folder_prefixes: &[String]) {
    // 曾在监控目录下、现已不在任何目录前缀内的 → missing
    let stale: Vec<String> = track_paths(conn)
        .into_iter()
        .map(|(p, _, _)| p)
        .filter(|p| {
            !seen.contains(p)
                && folder_prefixes
                    .iter()
                    .any(|f| p.starts_with(f.as_str()))
        })
        .collect();
    for p in &stale {
        let _ = conn.execute(
            "UPDATE tracks SET missing = 1 WHERE path = ?1",
            params![p],
        );
    }
    // 目录重新添加/文件回归：复活对应记录
    let revived: Vec<String> = seen
        .iter()
        .filter(|p| {
            folder_prefixes
                .iter()
                .any(|f| p.starts_with(f.as_str()))
        })
        .cloned()
        .collect();
    for p in &revived {
        let _ = conn.execute(
            "UPDATE tracks SET missing = 0 WHERE path = ?1",
            params![p],
        );
    }
}

fn row_to_meta(r: &Row) -> rusqlite::Result<TrackMeta> {
    Ok(TrackMeta {
        id: r.get(0)?,
        path: r.get(1)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album: r.get(4)?,
        album_artist: r.get(5)?,
        track_no: r.get(6)?,
        disc: r.get(7)?,
        year: r.get(8)?,
        duration: r.get(9)?,
        format: r.get(10)?,
        bitrate: r.get(11)?,
        sample_rate: r.get(12)?,
        bit_depth: r.get(13)?,
        cover: r.get(14)?,
        has_lrc: !r.get::<_, String>(15)?.is_empty(),
        size: r.get(16)?,
        mtime: r.get(17)?,
        liked: r.get::<_, i64>(18)? != 0,
        play_count: r.get(19)?,
        last_played: r.get(20)?,
        missing: r.get::<_, i64>(21)? != 0,
        liked_at: r.get(22)?,
    })
}

const TRACK_SELECT: &str = r#"
SELECT t.id, t.path, t.title, t.artist, t.album, t.album_artist, t.track_no, t.disc, t.year,
       t.duration, t.format, t.bitrate, t.sample_rate, t.bit_depth, t.cover, t.lrc_path,
       t.size, t.mtime,
       CASE WHEN l.track_id IS NULL THEN 0 ELSE 1 END,
       COALESCE(s.play_count, 0), COALESCE(s.last_played, 0), t.missing,
       COALESCE(l.liked_at, 0)
FROM tracks t
LEFT JOIN liked l ON l.track_id = t.id
LEFT JOIN stats s ON s.track_id = t.id
"#;

pub fn list_tracks(conn: &Connection) -> Vec<TrackMeta> {
    // 用户主动移除的歌曲不出现在资料库（get_track 仍保留，歌单引用可继续播放）
    let mut stmt = match conn.prepare(&(TRACK_SELECT.to_string() + "WHERE t.removed = 0 ORDER BY t.id")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[db] 读取曲目列表失败: {e}");
            return vec![];
        }
    };
    stmt.query_map([], row_to_meta)
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

pub fn get_track(conn: &Connection, id: i64) -> Option<TrackMeta> {
    conn.query_row(
        &(TRACK_SELECT.to_string() + "WHERE t.id = ?1"),
        params![id],
        row_to_meta,
    )
    .optional()
    .ok()
    .flatten()
}

pub fn get_track_path(conn: &Connection, id: i64) -> Option<String> {
    conn.query_row("SELECT path FROM tracks WHERE id = ?1", params![id], |r| {
        r.get(0)
    })
    .optional()
    .ok()
    .flatten()
}

pub fn get_lrc_path(conn: &Connection, id: i64) -> Option<String> {
    let v: Option<String> = conn
        .query_row("SELECT lrc_path FROM tracks WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten();
    v.filter(|s| !s.is_empty())
}

pub fn like_track(conn: &Connection, id: i64, on: bool) {
    if on {
        // 重复喜欢不刷新时间戳（ON CONFLICT DO NOTHING），保持首次喜欢时间
        let _ = conn.execute(
            "INSERT INTO liked(track_id, liked_at) VALUES(?1, ?2)
             ON CONFLICT(track_id) DO NOTHING",
            params![id, now_secs()],
        );
    } else {
        let _ = conn.execute("DELETE FROM liked WHERE track_id = ?1", params![id]);
    }
}

pub fn record_play(conn: &Connection, id: i64) {
    let _ = conn.execute(
        "INSERT INTO stats(track_id, play_count, last_played) VALUES(?1, 1, ?2)
         ON CONFLICT(track_id) DO UPDATE SET play_count = play_count + 1, last_played = ?2",
        params![id, now_secs()],
    );
}

// ---------- playlists ----------

pub fn list_playlists(conn: &Connection) -> Vec<Playlist> {
    let mut stmt = match conn.prepare(
        "SELECT id, name, created_at, remote_kind, remote_pid, origin_name
         FROM playlists ORDER BY sort_pos, id",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let mut out: Vec<Playlist> = stmt
        .query_map([], |r| {
            Ok(Playlist {
                id: r.get(0)?,
                name: r.get(1)?,
                track_ids: vec![],
                entries: vec![],
                cover: String::new(),
                created_at: r.get(2)?,
                remote_kind: r.get(3)?,
                remote_pid: r.get(4)?,
                origin_name: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    // 一次查完所有歌单的条目再分组，避免"每个歌单查一次"的 N+1
    let by_playlist = playlist_entries_grouped(conn);

    for pl in out.iter_mut() {
        let Some(entries) = by_playlist.get(&pl.id) else { continue };
        for e in entries {
            if pl.cover.is_empty() && !e.cover.is_empty() {
                pl.cover = e.cover.clone();
            }
            if e.kind == "local" {
                pl.track_ids.push(e.track_id);
            }
            pl.entries.push(crate::models::PlaylistEntryMeta {
                rowid: e.rowid,
                kind: e.kind.clone(),
                track_id: (e.kind == "local").then_some(e.track_id),
                online_id: (e.kind != "local").then_some(e.online_id.clone()),
                title: e.title.clone(),
                artist: e.artist.clone(),
                album: e.album.clone(),
                cover: e.cover.clone(),
                duration: e.duration,
                media_mid: e.media_mid.clone(),
                vip: e.vip,
                last_played: e.last_played,
                liked_at: e.liked_at,
            });
        }
    }
    out
}

pub fn create_playlist(conn: &Connection, name: &str) -> Result<i64, String> {
    conn.execute(
        "INSERT INTO playlists(name, created_at, sort_pos)
         VALUES(?1, ?2, COALESCE((SELECT MAX(sort_pos) FROM playlists), 0) + 1)",
        params![name, now_secs()],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// 按远程歌单标识找本地播放列表（重复导入合并用）：
/// 优先远程 id；旧版本导入的列表没存远程 id，退化为同名/原始名匹配
pub fn find_playlist_by_remote(
    conn: &Connection,
    kind: &str,
    remote_pid: &str,
    name: &str,
) -> Option<i64> {
    let by_remote: Option<i64> = conn
        .query_row(
            "SELECT id FROM playlists WHERE remote_kind = ?1 AND remote_pid = ?2 ORDER BY id LIMIT 1",
            params![kind, remote_pid],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if by_remote.is_some() {
        return by_remote;
    }
    // 同名或原始名匹配（origin_name = 导入时记录的远程歌单名，
    // 列表被改名后仍能靠它认出）
    conn.query_row(
        "SELECT id FROM playlists
         WHERE remote_kind = '' AND (name = ?1 OR (origin_name != '' AND origin_name = ?1))
         ORDER BY id LIMIT 1",
        params![name],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// 记录/补全播放列表的远程歌单标识（同名匹配到的旧列表在此回填）
pub fn set_playlist_remote(conn: &Connection, id: i64, kind: &str, remote_pid: &str) {
    let _ = conn.execute(
        "UPDATE playlists SET remote_kind = ?2, remote_pid = ?3 WHERE id = ?1",
        params![id, kind, remote_pid],
    );
}

/// 补全播放列表的原始导入名（已设置过的不覆盖：用户改名后原值仍在）
pub fn set_playlist_origin(conn: &Connection, id: i64, name: &str) {
    if name.is_empty() {
        return;
    }
    let _ = conn.execute(
        "UPDATE playlists SET origin_name = ?2 WHERE id = ?1 AND origin_name = ''",
        params![id, name],
    );
}

pub fn delete_playlist(conn: &Connection, id: i64) {
    let _ = conn.execute("DELETE FROM playlists WHERE id = ?1", params![id]);
    let _ = conn.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", params![id]);
}

/// 重命名播放列表。远程导入的列表若还没记录原始导入名，先以当前名
///（即当初的导入名）回填 origin_name，再写入新名——改名不影响重导入合并。
pub fn rename_playlist(conn: &Connection, id: i64, name: &str) {
    let _ = conn.execute(
        "UPDATE playlists SET
           origin_name = CASE WHEN origin_name = '' AND remote_pid != '' THEN name ELSE origin_name END,
           name = ?2
         WHERE id = ?1",
        params![id, name],
    );
}

/// 播放列表手动排序：按传入的 id 序列重写 sort_pos（1 起）
pub fn reorder_playlists(conn: &Connection, ids: &[i64]) {
    for (i, id) in ids.iter().enumerate() {
        let _ = conn.execute(
            "UPDATE playlists SET sort_pos = ?1 WHERE id = ?2",
            params![(i + 1) as i64, id],
        );
    }
}

pub fn add_online_to_playlist(
    conn: &Connection,
    pid: i64,
    kind: &str,
    rid: &str,
) -> Result<bool, String> {
    Ok(add_playlist_entry(conn, pid, kind, 0, rid))
}

pub fn add_to_playlist(conn: &Connection, pid: i64, tid: i64) -> Result<(), String> {
    let pos: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM playlist_tracks WHERE playlist_id = ?1",
            params![pid],
            |r| r.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT OR IGNORE INTO playlist_tracks(playlist_id, track_id, position) VALUES(?1, ?2, ?3)",
        params![pid, tid, pos],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn remove_from_playlist(conn: &Connection, pid: i64, tid: i64) {
    let _ = conn.execute(
        "DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND track_id = ?2",
        params![pid, tid],
    );
}

// ---------- sources ----------

pub fn list_sources(conn: &Connection) -> Vec<SourceItem> {
    let mut stmt = match conn.prepare("SELECT id, url, title, created_at FROM sources ORDER BY id DESC") {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |r| {
        Ok(SourceItem { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, created_at: r.get(3)? })
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

pub fn add_source(conn: &Connection, url: &str, title: &str) -> Result<i64, String> {
    conn.execute(
        "INSERT OR IGNORE INTO sources(url, title, created_at) VALUES(?1, ?2, ?3)",
        params![url, title, now_secs()],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_source(conn: &Connection, id: i64) {
    let _ = conn.execute("DELETE FROM sources WHERE id = ?1", params![id]);
}

pub fn get_source(conn: &Connection, id: i64) -> Option<SourceItem> {
    conn.query_row(
        "SELECT id, url, title, created_at FROM sources WHERE id = ?1",
        params![id],
        |r| {
            Ok(SourceItem {
                id: r.get(0)?,
                url: r.get(1)?,
                title: r.get(2)?,
                created_at: r.get(3)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

// ---------- 音源管理（LX 兼容脚本 / 网络接口音源） ----------

fn row_to_lx_source(r: &Row) -> rusqlite::Result<LxSourceItem> {
    let platforms_json: String = r.get(5)?;
    Ok(LxSourceItem {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        base_url: r.get(3)?,
        origin: r.get(4)?,
        platforms: serde_json::from_str(&platforms_json).unwrap_or_default(),
        enabled: r.get::<_, i64>(6)? != 0,
        api_mode: r.get(7)?,
        created_at: r.get(8)?,
    })
}

pub fn lx_list_sources(conn: &Connection) -> Vec<LxSourceItem> {
    let mut stmt = match conn.prepare(
        "SELECT id, kind, name, base_url, origin, platforms, enabled, api_mode, created_at \
         FROM lx_sources ORDER BY id DESC",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], row_to_lx_source)
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// 仅取启用的源（后续播放链路分流用）
pub fn lx_enabled_sources(conn: &Connection) -> Vec<LxSourceItem> {
    lx_list_sources(conn)
        .into_iter()
        .filter(|s| s.enabled)
        .collect()
}

/// 插入；同 base_url 已存在时返回该行 id（幂等）。
/// 返回 (id, 是否新建)
pub fn lx_add_source(
    conn: &Connection,
    kind: &str,
    name: &str,
    base_url: &str,
    origin: &str,
    platforms_json: &str,
    api_mode: &str,
) -> Result<(i64, bool), String> {
    if let Some(existing) = lx_list_sources(conn).into_iter().find(|s| s.base_url == base_url) {
        // 已存在：刷新契约与脚本原文（订阅可能更新），保持启用状态不变
        conn.execute(
            "UPDATE lx_sources SET name=?1, origin=?2, platforms=?3, kind=?4, api_mode=?5 WHERE id=?6",
            params![name, origin, platforms_json, kind, api_mode, existing.id],
        )
        .map_err(|e| e.to_string())?;
        return Ok((existing.id, false));
    }
    conn.execute(
        "INSERT INTO lx_sources(kind, name, base_url, origin, platforms, enabled, api_mode, created_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
        params![kind, name, base_url, origin, platforms_json, api_mode, now_secs()],
    )
    .map_err(|e| e.to_string())?;
    Ok((conn.last_insert_rowid(), true))
}

pub fn lx_set_enabled(conn: &Connection, id: i64, enabled: bool) -> Result<(), String> {
    conn.execute(
        "UPDATE lx_sources SET enabled=?1 WHERE id=?2",
        params![enabled as i64, id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn lx_delete_source(conn: &Connection, id: i64) {
    let _ = conn.execute("DELETE FROM lx_sources WHERE id = ?1", params![id]);
}

pub fn update_source_title(conn: &Connection, id: i64, title: &str) {
    let _ = conn.execute("UPDATE sources SET title = ?2 WHERE id = ?1", params![id, title]);
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------- 在线曲目（网易云 / QQ 音乐条目入库） ----------

pub fn upsert_online_track(
    conn: &Connection,
    kind: &str,
    rid: &str,
    title: &str,
    artist: &str,
    album: &str,
    cover: &str,
    duration_ms: i64,
    media_mid: &str,
    vip: bool,
) -> i64 {
    let _ = conn.execute(
        "INSERT INTO online_tracks(kind, rid, title, artist, album, cover, duration_ms, media_mid, vip)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(kind, rid) DO UPDATE SET
           title=?3, artist=?4, album=?5, cover=?6, duration_ms=?7, media_mid=?8, vip=?9",
        params![kind, rid, title, artist, album, cover, duration_ms, media_mid, vip as i64],
    );
    conn.last_insert_rowid()
}

/// 记录在线曲目播放（未入库的自动补录元数据，供“最近播放”显示）；
/// INSERT OR IGNORE：已存在（如之前收藏过）不覆盖其 vip/封面等元数据
pub fn record_play_online(conn: &Connection, kind: &str, rid: &str, title: &str, artist: &str, album: &str, cover: &str, duration_ms: i64, media_mid: &str, vip: bool) {
    let _ = conn.execute(
        "INSERT OR IGNORE INTO online_tracks(kind, rid, title, artist, album, cover, duration_ms, media_mid, vip)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![kind, rid, title, artist, album, cover, duration_ms, media_mid, vip as i64],
    );
    if let Err(e) = conn.execute(
        "UPDATE online_tracks SET last_played = ?3, play_count = play_count + 1 WHERE kind = ?1 AND rid = ?2",
        params![kind, rid, now_secs()],
    ) {
        eprintln!("[db] 记录在线播放失败 (kind={kind}, rid={rid}): {e}");
    }
}

/// “最近播放”的在线曲目部分（有播放记录的），按最近播放时间倒序
pub fn recent_online_list(conn: &Connection, limit: i64) -> Vec<PlaylistEntryRow> {
    let mut stmt = match conn.prepare(
        "SELECT kind, rid, title, artist, album, cover, duration_ms, media_mid, vip, last_played
         FROM online_tracks
         WHERE last_played > 0
         ORDER BY last_played DESC
         LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[db] 读取在线最近播放失败: {e}");
            return vec![];
        }
    };
    stmt.query_map([limit], |r| {
        Ok(PlaylistEntryRow {
            rowid: 0,
            kind: r.get(0)?,
            track_id: 0,
            online_id: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            album: r.get(4)?,
            cover: r.get(5)?,
            duration: r.get::<_, i64>(6).unwrap_or(0) as f64 / 1000.0,
            media_mid: r.get(7)?,
            vip: r.get::<_, i64>(8)? != 0,
            last_played: r.get::<_, i64>(9).unwrap_or(0),
            liked_at: 0,
        })
    })
    .map(|rows| rows.filter_map(|x| x.ok()).collect())
    .unwrap_or_default()
}

// ---------- 播放列表条目（本地 + 在线混合） ----------

#[derive(Debug, Clone)]
pub struct PlaylistEntryRow {
    pub rowid: i64,
    pub kind: String,
    pub track_id: i64,
    pub online_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
    pub duration: f64,
    pub media_mid: String,
    pub vip: bool,
    /// 最近播放时间（unix 秒；“最近播放”合并排序用，0 = 无记录）
    pub last_played: i64,
    /// 收藏时间（unix 秒；“我喜欢”合并排序用，0 = 无记录）
    pub liked_at: i64,
}

/// 歌单条目的 JOIN 查询。`WHERE` 子句由调用方拼接（单列表按 id 过滤 / 全量分组）。
const PLAYLIST_ENTRIES_SQL: &str = "SELECT pt.playlist_id, pt.rowid, pt.kind, pt.track_id, pt.online_id,
              COALESCE(t.title, ot.title, '') AS title,
              COALESCE(t.artist, ot.artist, '') AS artist,
              COALESCE(t.album, ot.album, '') AS album,
              COALESCE(t.cover, ot.cover, '') AS cover,
              COALESCE(t.duration, ot.duration_ms / 1000.0, 0) AS duration,
              COALESCE(ot.media_mid, '') AS media_mid,
              COALESCE(ot.vip, 0) AS vip,
              COALESCE(ot.last_played, 0) AS last_played,
              COALESCE(l.liked_at, lo.liked_at, 0) AS liked_at
             FROM playlist_tracks pt
             LEFT JOIN tracks t ON pt.kind = 'local' AND t.id = pt.track_id
             LEFT JOIN online_tracks ot ON pt.kind != 'local' AND ot.kind = pt.kind AND ot.rid = pt.online_id
             LEFT JOIN liked l ON pt.kind = 'local' AND l.track_id = pt.track_id
             LEFT JOIN liked_online lo ON pt.kind != 'local' AND lo.kind = pt.kind AND lo.rid = pt.online_id";

/// 把 JOIN 结果行映射成 (playlist_id, PlaylistEntryRow)
fn map_entry(r: &Row) -> rusqlite::Result<(i64, PlaylistEntryRow)> {
    Ok((
        r.get(0)?,
        PlaylistEntryRow {
            rowid: r.get(1)?,
            kind: r.get(2)?,
            track_id: r.get(3)?,
            online_id: r.get(4)?,
            title: r.get(5)?,
            artist: r.get(6)?,
            album: r.get(7)?,
            cover: r.get(8)?,
            duration: r.get(9)?,
            media_mid: r.get(10)?,
            vip: r.get::<_, i64>(11)? != 0,
            last_played: r.get::<_, i64>(12).unwrap_or(0),
            liked_at: r.get::<_, i64>(13).unwrap_or(0),
        },
    ))
}

/// 取单个歌单的条目。生产路径走 playlist_entries_grouped（list_playlists 一次查完），
/// 这里保留给按需单查的场合与测试。
#[cfg_attr(not(test), allow(dead_code))]
pub fn playlist_entries(conn: &Connection, pid: i64) -> Vec<PlaylistEntryRow> {
    let sql = format!("{PLAYLIST_ENTRIES_SQL} WHERE pt.playlist_id = ?1 ORDER BY pt.position, pt.rowid");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            // 读取失败时明确日志：列表“空但导入成功”这类表象的根因都在这里
            eprintln!("[db] 读取播放列表条目失败 (playlist={pid}): {e}");
            return vec![];
        }
    };
    stmt.query_map(params![pid], |r| map_entry(r).map(|(_, e)| e))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// 一次查出所有歌单的条目并按 playlist_id 分组。
/// list_playlists 用它替代”每个歌单查一次”，避免歌单变多时的 N+1。
fn playlist_entries_grouped(
    conn: &Connection,
) -> std::collections::HashMap<i64, Vec<PlaylistEntryRow>> {
    let mut out: std::collections::HashMap<i64, Vec<PlaylistEntryRow>> =
        std::collections::HashMap::new();
    let sql = format!(
        "{PLAYLIST_ENTRIES_SQL} ORDER BY pt.playlist_id, pt.position, pt.rowid"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[db] 读取播放列表条目失败: {e}");
            return out;
        }
    };
    let rows = match stmt.query_map([], map_entry) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[db] 读取播放列表条目失败: {e}");
            return out;
        }
    };
    for r in rows.filter_map(|r| r.ok()) {
        out.entry(r.0).or_default().push(r.1);
    }
    out
}

/// 追加条目到播放列表末尾；已存在（主键冲突）时跳过。
/// 返回是否真的新增（导入合并时的“新增 N 首”计数用）
pub fn add_playlist_entry(conn: &Connection, pid: i64, kind: &str, track_id: i64, online_id: &str) -> bool {
    let pos: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM playlist_tracks WHERE playlist_id = ?1",
            params![pid],
            |r| r.get(0),
        )
        .unwrap_or(1);
    conn.execute(
        "INSERT OR IGNORE INTO playlist_tracks(playlist_id, track_id, position, kind, online_id) VALUES(?1,?2,?3,?4,?5)",
        params![pid, track_id, pos, kind, online_id],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn remove_playlist_entry(conn: &Connection, rowid: i64) {
    let _ = conn.execute("DELETE FROM playlist_tracks WHERE rowid = ?1", params![rowid]);
}

// ---------- 在线喜欢（轻量引用，不下载） ----------

pub fn like_online_track(conn: &Connection, kind: &str, rid: &str) {
    let _ = conn.execute(
        "INSERT OR IGNORE INTO liked_online(kind, rid, liked_at) VALUES(?1, ?2, ?3)",
        params![kind, rid, now_secs()],
    );
}

pub fn unlike_online_track(conn: &Connection, kind: &str, rid: &str) {
    let _ = conn.execute(
        "DELETE FROM liked_online WHERE kind = ?1 AND rid = ?2",
        params![kind, rid],
    );
}

pub fn liked_online_list(conn: &Connection) -> Vec<PlaylistEntryRow> {
    let mut stmt = match conn.prepare(
        "SELECT l.kind, l.rid, ot.title, ot.artist, ot.album, ot.cover, ot.duration_ms, ot.media_mid, ot.vip, l.liked_at
         FROM liked_online l
         LEFT JOIN online_tracks ot ON ot.kind = l.kind AND ot.rid = l.rid
         ORDER BY l.liked_at DESC, l.rowid DESC",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([], |r| {
        Ok(PlaylistEntryRow {
            rowid: 0,
            kind: r.get(0)?,
            track_id: 0,
            online_id: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            album: r.get(4)?,
            cover: r.get(5)?,
            duration: r.get::<_, i64>(6).unwrap_or(0) as f64 / 1000.0,
            media_mid: r.get(7)?,
            vip: r.get::<_, i64>(8)? != 0,
            last_played: 0,
            liked_at: r.get::<_, i64>(9).unwrap_or(0),
        })
    })
    .map(|rows| rows.filter_map(|x| x.ok()).collect())
    .unwrap_or_default()
}

#[allow(dead_code)]
pub fn is_liked_online(conn: &Connection, kind: &str, rid: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM liked_online WHERE kind = ?1 AND rid = ?2",
        params![kind, rid],
        |_| Ok(()),
    )
    .is_ok()
}

pub fn mark_online_downloaded(conn: &Connection, kind: &str, rid: &str) {
    let _ = conn.execute(
        "UPDATE online_tracks SET downloaded = 1 WHERE kind = ?1 AND rid = ?2",
        params![kind, rid],
    );
}

/// 取在线条目存的封面 URL
pub fn get_online_cover(conn: &Connection, kind: &str, rid: &str) -> Option<String> {
    conn.query_row(
        "SELECT cover FROM online_tracks WHERE kind = ?1 AND rid = ?2",
        params![kind, rid],
        |r| r.get(0),
    )
    .ok()
    .filter(|s: &String| !s.is_empty())
}

// ---------- 手动排序（“资料库/我喜欢”共用；播放列表走 playlist_tracks.position） ----------

/// 保存一份列表的完整手动顺序（全量覆盖，行 key 序列即顺序）。
/// list: "library" | "liked"；row_key: 本地 "track:<id>" / 在线 "netease:<rid>" / "qq:<rid>"
///（与 unavailable 键的格式一致）。未包含的 key（新入库/新收藏）自动排到已排序项之后。
pub fn save_manual_order(conn: &Connection, list: &str, keys: &[String]) {
    let _ = conn.execute("DELETE FROM manual_order WHERE list = ?1", params![list]);
    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[db] 手动排序事务失败: {e}");
            return;
        }
    };
    for (i, k) in keys.iter().enumerate() {
        let _ = tx.execute(
            "INSERT OR REPLACE INTO manual_order(list, row_key, pos) VALUES(?1, ?2, ?3)",
            params![list, k, (i + 1) as i64],
        );
    }
    if let Err(e) = tx.commit() {
        eprintln!("[db] 保存手动排序失败: {e}");
    }
}

/// 读手动排序（row_key → 序号，1 起；无记录的 key 视为 0，排已排序项之后）
pub fn manual_order_map(conn: &Connection, list: &str) -> std::collections::HashMap<String, i64> {
    let mut out = std::collections::HashMap::new();
    let mut stmt = match conn.prepare("SELECT row_key, pos FROM manual_order WHERE list = ?1") {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows: Vec<(String, i64)> = stmt
        .query_map(params![list], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    for (k, p) in rows {
        out.insert(k, p);
    }
    out
}

/// 播放列表条目手动排序：按给定 rowid 序列重写 position（全量覆盖）。
/// 前端总是提交整份序列（含搜索时不可见的行），未提到的行保持原位。
pub fn reorder_playlist(conn: &Connection, pid: i64, rowids: &[i64]) {
    for (i, rid) in rowids.iter().enumerate() {
        let _ = conn.execute(
            "UPDATE playlist_tracks SET position = ?3 WHERE rowid = ?2 AND playlist_id = ?1",
            params![pid, rid, (i + 1) as i64],
        );
    }
}

// ---------- 下载管理 ----------

/// 一条下载任务（列表页的一行）
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadTask {
    pub id: String,
    pub kind: String,
    pub song_id: String,
    pub media_mid: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
    pub size: i64,
    pub received: i64,
    /// queued | downloading | done | failed
    pub status: String,
    pub error: String,
    pub file_path: String,
    pub created_at: i64,
    pub finished_at: i64,
}

const DL_COLS: &str = "id, kind, song_id, media_mid, title, artist, album, cover, \
     size, received, status, error, file_path, created_at, finished_at";

fn dl_from_row(r: &Row) -> rusqlite::Result<DownloadTask> {
    Ok(DownloadTask {
        id: r.get(0)?,
        kind: r.get(1)?,
        song_id: r.get(2)?,
        media_mid: r.get(3)?,
        title: r.get(4)?,
        artist: r.get(5)?,
        album: r.get(6)?,
        cover: r.get(7)?,
        size: r.get(8)?,
        received: r.get(9)?,
        status: r.get(10)?,
        error: r.get(11)?,
        file_path: r.get(12)?,
        created_at: r.get(13)?,
        finished_at: r.get(14)?,
    })
}

/// 列出任务。`status` 为空时返回全部，按创建时间倒序（新的在前）。
pub fn list_download_tasks(conn: &Connection, status: &str) -> Vec<DownloadTask> {
    let sql = if status.is_empty() {
        format!("SELECT {DL_COLS} FROM download_tasks ORDER BY created_at DESC, rowid DESC")
    } else {
        format!("SELECT {DL_COLS} FROM download_tasks WHERE status = ?1 ORDER BY created_at DESC, rowid DESC")
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[db] 读取下载任务失败: {e}");
            return vec![];
        }
    };
    let rows = if status.is_empty() {
        stmt.query_map([], dl_from_row)
    } else {
        stmt.query_map(params![status], dl_from_row)
    };
    match rows {
        Ok(r) => r.filter_map(|x| x.ok()).collect(),
        Err(e) => {
            eprintln!("[db] 读取下载任务失败: {e}");
            vec![]
        }
    }
}

/// 登记/覆盖一个下载任务（按 id 幂等）。已完成的重复点击不再重置为 queued。
pub fn upsert_download_task(conn: &Connection, t: &DownloadTask) {
    let _ = conn.execute(
        "INSERT INTO download_tasks
           (id, kind, song_id, media_mid, title, artist, album, cover,
            size, received, status, error, file_path, created_at, finished_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         ON CONFLICT(id) DO UPDATE SET
           title=excluded.title, artist=excluded.artist, album=excluded.album,
           cover=excluded.cover, media_mid=excluded.media_mid,
           size=CASE WHEN excluded.size > 0 THEN excluded.size ELSE download_tasks.size END,
           status=excluded.status, error=excluded.error,
           file_path=excluded.file_path, received=excluded.received,
           finished_at=excluded.finished_at",
        params![
            t.id, t.kind, t.song_id, t.media_mid, t.title, t.artist, t.album, t.cover,
            t.size, t.received, t.status, t.error, t.file_path, t.created_at, t.finished_at,
        ],
    );
}

/// 只更新进度与状态，避免整行覆写时把标题等字段清空。
pub fn update_download_progress(conn: &Connection, id: &str, received: i64, total: i64) {
    let _ = conn.execute(
        "UPDATE download_tasks SET received = ?2,
           size = CASE WHEN ?3 > 0 THEN ?3 ELSE size END
         WHERE id = ?1",
        params![id, received, total],
    );
}

pub fn set_download_status(conn: &Connection, id: &str, status: &str, error: &str, file_path: &str) {
    let _ = conn.execute(
        "UPDATE download_tasks
            SET status = ?2, error = ?3,
                file_path = CASE WHEN ?4 != '' THEN ?4 ELSE file_path END,
                finished_at = CASE WHEN ?2 = 'done' THEN ?5 ELSE finished_at END
          WHERE id = ?1",
        params![id, status, error, file_path, now_secs()],
    );
}

pub fn get_download_task(conn: &Connection, id: &str) -> Option<DownloadTask> {
    conn.query_row(
        &format!("SELECT {DL_COLS} FROM download_tasks WHERE id = ?1"),
        params![id],
        dl_from_row,
    )
    .ok()
}

/// 删除任务记录。`done` 为 true 时一并删除磁盘文件（仅限仍在本任务记录里的成品）。
pub fn delete_download_task(conn: &Connection, id: &str, done: bool) {
    if done {
        if let Some(t) = get_download_task(conn, id) {
            if t.status == "done" && !t.file_path.is_empty() {
                let _ = std::fs::remove_file(&t.file_path);
            }
        }
    }
    let _ = conn.execute("DELETE FROM download_tasks WHERE id = ?1", params![id]);
}

/// 清空某个状态的所有任务（"全部删除"用）。返回被删记录的磁盘文件路径。
pub fn clear_download_tasks(conn: &Connection, status: &str, delete_files: bool) -> Vec<String> {
    let sql = if status.is_empty() {
        "SELECT id, status, file_path FROM download_tasks"
    } else {
        "SELECT id, status, file_path FROM download_tasks WHERE status = ?1"
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[db] 清理下载任务失败: {e}");
            return vec![];
        }
    };
    let map = |r: &Row| -> rusqlite::Result<(String, String, String)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    };
    // 先把要删的行读出来再删：直接边遍历边删会与本语句的游标冲突
    let items: Vec<(String, String, String)> = if status.is_empty() {
        stmt.query_map([], map)
            .map(|r| r.flatten().collect())
            .unwrap_or_default()
    } else {
        stmt.query_map(params![status], map)
            .map(|r| r.flatten().collect())
            .unwrap_or_default()
    };
    drop(stmt);
    let mut files = vec![];
    for (id, st, path) in items {
        if delete_files && st == "done" && !path.is_empty() {
            let _ = std::fs::remove_file(&path);
            files.push(path);
        }
        let _ = conn.execute("DELETE FROM download_tasks WHERE id = ?1", params![id]);
    }
    files
}

/// 启动时补登记：右键"下载到本地"/播放条直下的歌曲此前从不写任务表，
/// 下载管理里看不到，"打开所在位置"也无从指向真实目录。
/// 把保存目录第一层、任务表还没有记录的成品文件补成 done 行（幂等）。
pub fn backfill_download_tasks(conn: &Connection, save_dir: &std::path::Path) {
    let dir_norm = save_dir
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_string();
    let mut existing: HashSet<String> = HashSet::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT file_path FROM download_tasks WHERE file_path != ''")
    {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                existing.insert(p);
            }
        }
    }
    let mut added = 0;
    for t in list_tracks(conn) {
        if t.missing || existing.contains(&t.path) {
            continue;
        }
        let p = std::path::Path::new(&t.path);
        // 只补保存目录第一层：直下产物都平铺在这里，子目录是用户自己整理的
        let Some(parent) = p.parent() else { continue };
        let parent_str = parent.to_string_lossy();
        let parent_norm = parent_str.trim_end_matches(['\\', '/']);
        if !parent_norm.eq_ignore_ascii_case(&dir_norm) || !p.is_file() {
            continue;
        }
        let ts = if t.mtime > 0 { t.mtime } else { now_secs() };
        upsert_download_task(
            conn,
            &DownloadTask {
                id: format!("file:{}", t.id),
                kind: "local".into(),
                song_id: t.id.to_string(),
                media_mid: String::new(),
                title: t.title,
                artist: t.artist,
                album: t.album,
                cover: t.cover,
                size: t.size,
                received: t.size,
                status: "done".into(),
                error: String::new(),
                file_path: t.path,
                created_at: ts,
                finished_at: ts,
            },
        );
        added += 1;
    }
    if added > 0 {
        eprintln!("[db] 补登记下载任务 {added} 条");
    }
}

/// 统计各状态数量，供列表页头部展示。
#[allow(dead_code)]
pub fn download_counts(conn: &Connection) -> std::collections::HashMap<String, i64> {
    let mut m = std::collections::HashMap::new();
    if let Ok(mut s) = conn.prepare("SELECT status, COUNT(*) FROM download_tasks GROUP BY status") {
        if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
            for (k, v) in rows.flatten() {
                m.insert(k, v);
            }
        }
    }
    m
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// 线上曾出现的旧 schema：online_tracks 只有 downloaded（无 last_played/play_count），
    /// tracks 无 missing——migrate() 曾因 execute_batch 首条失败而整批中止，后续列
    /// 永远补不上，导致 playlist_entries/recent_online_list/list_tracks 静默返回空。
    /// 此测试锁定该升级路径：旧库 init() 后上述查询必须全部可用。
    #[test]
    fn legacy_schema_upgrades_and_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        // 1) 旧库 schema（含 v0.1.0 首版就有、会触发“列已存在”的 downloaded 列）
        conn.execute_batch(
            r#"
            CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE folders (id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT UNIQUE NOT NULL);
            CREATE TABLE tracks (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              path TEXT UNIQUE NOT NULL,
              title TEXT NOT NULL DEFAULT '',
              artist TEXT NOT NULL DEFAULT '',
              album TEXT NOT NULL DEFAULT '',
              album_artist TEXT NOT NULL DEFAULT '',
              track_no INTEGER NOT NULL DEFAULT 0,
              disc INTEGER NOT NULL DEFAULT 0,
              year INTEGER NOT NULL DEFAULT 0,
              duration REAL NOT NULL DEFAULT 0,
              format TEXT NOT NULL DEFAULT '',
              bitrate INTEGER NOT NULL DEFAULT 0,
              sample_rate INTEGER NOT NULL DEFAULT 0,
              bit_depth INTEGER NOT NULL DEFAULT 0,
              cover TEXT NOT NULL DEFAULT '',
              lrc_path TEXT NOT NULL DEFAULT '',
              size INTEGER NOT NULL DEFAULT 0,
              mtime INTEGER NOT NULL DEFAULT 0,
              added_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE playlists (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE playlist_tracks (
              playlist_id INTEGER NOT NULL,
              track_id INTEGER NOT NULL,
              position INTEGER NOT NULL DEFAULT 0,
              kind TEXT NOT NULL DEFAULT 'local',
              online_id TEXT NOT NULL DEFAULT '',
              PRIMARY KEY (playlist_id, kind, online_id, track_id)
            );
            CREATE TABLE sources (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT UNIQUE NOT NULL, title TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE stats (track_id INTEGER PRIMARY KEY, play_count INTEGER NOT NULL DEFAULT 0, last_played INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE liked (track_id INTEGER PRIMARY KEY);
            CREATE TABLE online_tracks (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              kind TEXT NOT NULL,
              rid TEXT NOT NULL,
              title TEXT NOT NULL DEFAULT '',
              artist TEXT NOT NULL DEFAULT '',
              album TEXT NOT NULL DEFAULT '',
              cover TEXT NOT NULL DEFAULT '',
              duration_ms INTEGER NOT NULL DEFAULT 0,
              media_mid TEXT NOT NULL DEFAULT '',
              vip INTEGER NOT NULL DEFAULT 0,
              downloaded INTEGER NOT NULL DEFAULT 0,
              UNIQUE(kind, rid)
            );
            CREATE TABLE liked_online (
              rowid INTEGER PRIMARY KEY AUTOINCREMENT,
              kind TEXT NOT NULL,
              rid TEXT NOT NULL,
              liked_at INTEGER NOT NULL DEFAULT 0,
              UNIQUE(kind, rid)
            );
            "#,
        )
        .unwrap();
        // 2) 旧库里的既有数据：导入过歌单的在线条目 + 本地曲目
        upsert_online_track(&conn, "qq", "001Song", "歌 A", "歌手", "专辑", "http://c/1.jpg", 200000, "M001", true);
        add_online_to_playlist(&conn, 1, "qq", "001Song").unwrap();
        upsert_online_track(&conn, "netease", "101Song", "歌 B", "歌手", "专辑", "http://c/2.jpg", 180000, "", false);
        add_online_to_playlist(&conn, 1, "netease", "101Song").unwrap();

        // 3) 升级（init 的 SCHEMA 是 CREATE IF NOT EXISTS，不会动旧表；migrate 负责补列）
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn);

        // 4) 三条曾静默失败的查询路径必须恢复
        // 4a) 歌单条目（歌单导入后“歌曲信息没被导入”的读取路径）
        let entries = playlist_entries(&conn, 1);
        assert_eq!(entries.len(), 2, "playlist_entries 应返回 2 条，实际 {entries:?}");
        assert_eq!(entries[0].title, "歌 A");
        assert_eq!(entries[1].title, "歌 B");
        assert_eq!(entries[0].vip, true);

        // 4b) 最近播放（在线）：记录后必须读得回来
        record_play_online(&conn, "qq", "001Song", "歌 A", "歌手", "专辑", "http://c/1.jpg", 200000, "M001", true);
        let recent = recent_online_list(&conn, 100);
        assert_eq!(recent.len(), 1, "recent_online_list 应返回 1 条，实际 {recent:?}");
        assert_eq!(recent[0].title, "歌 A");
        assert!(recent[0].last_played > 0);

        // 4c) 资料库列表（t.missing 列）
        let tracks = list_tracks(&conn);
        assert_eq!(tracks.len(), 0, "尚未入库本地曲目");

        // 5) 幂等：再次 migrate 不报错、不重复补列
        migrate(&conn);
        let entries2 = playlist_entries(&conn, 1);
        assert_eq!(entries2.len(), 2);
    }

    /// migrate 的 execute_batch 老写法回归：任何一条 ALTER 已存在即中止整批。
    /// 此测试保证新的逐列补列法即使某表已完全最新也能全部通过。
    #[test]
    fn migrate_is_idempotent_on_current_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn);
        migrate(&conn);
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('online_tracks')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"last_played".to_string()));
        assert!(cols.contains(&"play_count".to_string()));
        let one: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name='missing'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(one, 1);
    }

    /// 重复导入歌单的合并语义：
    /// 1) 已有条目不重复；2) 已有条目的顺序（position）不被改写；
    /// 3) 用户手动加进列表的其他歌不被删除；
    /// 4) 远程歌单新增的歌追加到本地列表末尾；
    /// 5) 按远程 id（优先）/同名（旧数据回退）找到已有列表而不是新建。
    #[test]
    fn reimport_playlist_merges_without_duplicates_or_reorder() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn);

        // 首次导入：远程歌单 3 首
        let pid = find_playlist_by_remote(&conn, "qq", "777", "我的最爱")
            .unwrap_or_else(|| create_playlist(&conn, "我的最爱").unwrap());
        set_playlist_remote(&conn, pid, "qq", "777");
        for rid in ["a", "b", "c"] {
            add_online_to_playlist(&conn, pid, "qq", rid).unwrap();
        }
        // 用户手动再往这个列表加两首（本地曲目 + 另一首在线）
        add_to_playlist(&conn, pid, 42).unwrap();
        add_online_to_playlist(&conn, pid, "netease", "n1").unwrap();
        // 用户手动调过顺序：把 c（position 序里的第 3 项）挪到最前
        let ids: Vec<i64> = playlist_entries(&conn, pid)
            .iter()
            .map(|e| e.rowid)
            .collect();
        let c_rowid = ids[2];
        let mut reordered: Vec<i64> = ids.iter().copied().filter(|r| *r != c_rowid).collect();
        reordered.insert(0, c_rowid); // c 放到最前
        reorder_playlist(&conn, pid, &reordered);

        // 远程歌单更新：少了一首 b，多了两首新歌 d/e（a、c 还在）
        let merged = find_playlist_by_remote(&conn, "qq", "777", "我的最爱").unwrap();
        assert_eq!(merged, pid, "同远程 id 应命中已有列表");
        let mut added = 0;
        for rid in ["a", "c", "d", "e"] {
            if add_online_to_playlist(&conn, pid, "qq", rid).unwrap() {
                added += 1;
            }
        }
        assert_eq!(added, 2, "只有 d/e 是新歌");

        // 最终列表：5 首原有（a c b 42 n1，c 在前）+ 2 首新歌在末尾；
        // 远程删掉的 b 仍保留（本地不追随远程删除）
        let entries = playlist_entries(&conn, pid);
        let seq: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.kind.clone(), e.online_id.clone()))
            .collect();
        assert_eq!(
            seq,
            vec![
                ("qq".into(), "c".into()),
                ("qq".into(), "a".into()),
                ("qq".into(), "b".into()),
                ("local".into(), "".into()),
                ("netease".into(), "n1".into()),
                ("qq".into(), "d".into()),
                ("qq".into(), "e".into()),
            ],
            "合并后顺序：手动序保持、手动加的歌保留、新歌在末尾，实际 {seq:?}"
        );

        // 同名回退：无远程标识的旧列表按名字匹配（旧版本导入的数据）
        let legacy = create_playlist(&conn, "我的最爱").unwrap();
        let hit = find_playlist_by_remote(&conn, "netease", "999", "我的最爱").unwrap();
        assert_eq!(hit, legacy, "无远程 id 时应同名匹配到旧列表");
        // 已带远程标识的列表不再参与同名匹配
        let miss = find_playlist_by_remote(&conn, "netease", "888", "不存在的名字");
        assert!(miss.is_none());
    }

    /// 补登记（backfill_download_tasks）只认保存目录第一层、真实存在、
    /// 且任务表里还没有的成品歌；重复执行幂等。下载管理的
    /// "打开所在位置"完全依赖这里的 file_path 指向真实保存目录。
    #[test]
    fn backfill_registers_save_dir_files_once() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        migrate(&conn);

        // 真实文件系统：保存目录第一层 / 子目录 / 目录外 / 有记录但文件不存在
        let tmp = std::env::temp_dir().join(format!("yimai_backfill_{}", std::process::id()));
        let save = tmp.join("下载目录");
        let sub = save.join("子目录");
        std::fs::create_dir_all(&sub).unwrap();
        let in_save = save.join("周传雄 - 黄昏.mp3");
        let in_sub = sub.join("深藏.flac");
        let outside = tmp.join("别处.mp3");
        let ghost = save.join("幽灵.mp3");
        std::fs::write(&in_save, b"x").unwrap();
        std::fs::write(&in_sub, b"x").unwrap();
        std::fs::write(&outside, b"x").unwrap();

        let mut ins = conn
            .prepare("INSERT INTO tracks(path, title, artist, size, mtime) VALUES(?1,?2,?3,7,123)")
            .unwrap();
        ins.execute(params![in_save.to_string_lossy().to_string(), "黄昏", "周传雄"])
            .unwrap();
        ins.execute(params![in_sub.to_string_lossy().to_string(), "深藏", ""])
            .unwrap();
        ins.execute(params![outside.to_string_lossy().to_string(), "别处", ""])
            .unwrap();
        ins.execute(params![ghost.to_string_lossy().to_string(), "幽灵", ""])
            .unwrap();
        drop(ins);

        backfill_download_tasks(&conn, &save);
        let rows = list_download_tasks(&conn, "");
        assert_eq!(
            rows.len(),
            1,
            "只有保存目录第一层真实存在的成品歌补登记：{rows:?}"
        );
        let r = &rows[0];
        assert_eq!(r.file_path, in_save.to_string_lossy());
        assert_eq!(r.status, "done");
        assert_eq!(r.kind, "local");
        assert!(r.id.starts_with("file:"));
        assert_eq!(r.size, 7);
        assert_eq!(r.created_at, 123, "创建时间取文件 mtime，列表按它倒序");

        // 幂等：重复跑不新增
        backfill_download_tasks(&conn, &save);
        assert_eq!(list_download_tasks(&conn, "").len(), 1);

        // file_path 已被任务表登记的（后来正常下载产生的行）跳过，不造重复行
        let in_save2 = save.join("迟志强 - 铁窗泪.mp3");
        std::fs::write(&in_save2, b"x").unwrap();
        conn.execute(
            "INSERT INTO tracks(path, title, artist, size, mtime) VALUES(?1,?2,?3,7,124)",
            params![in_save2.to_string_lossy().to_string(), "铁窗泪", "迟志强"],
        )
        .unwrap();
        upsert_download_task(
            &conn,
            &DownloadTask {
                id: "netease:999".into(),
                kind: "netease".into(),
                song_id: "999".into(),
                media_mid: String::new(),
                title: "铁窗泪".into(),
                artist: "迟志强".into(),
                album: String::new(),
                cover: String::new(),
                size: 7,
                received: 7,
                status: "done".into(),
                error: String::new(),
                file_path: in_save2.to_string_lossy().into_owned(),
                created_at: 1,
                finished_at: 1,
            },
        );
        backfill_download_tasks(&conn, &save);
        assert_eq!(
            list_download_tasks(&conn, "").len(),
            2,
            "file_path 已有任务行的文件不再补登记"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
