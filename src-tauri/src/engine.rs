use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use lofty::prelude::*;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::eq::{EqShared, EqSource};
use crate::smtc::SmtcMsg;

/// 解码 IO 缓冲：1 MiB（std 默认 8 KB）。解码线程在 cpal 回调驱动下持续拉样本，
/// 大文件（FLAC 几十 MB）小缓冲会频繁触发系统调用，偶发 underrun 造成卡顿/爆音；
/// 扩容后每次读取覆盖更长时间（1MiB ≈ 44.1kHz/16bit 立体声 ~7 秒），显著降低饥饿概率。
const DECODE_BUF_SIZE: usize = 1024 * 1024;

fn buffered_reader(file: File) -> BufReader<File> {
    BufReader::with_capacity(DECODE_BUF_SIZE, file)
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub id: Option<i64>,
    /// "track"（本地曲目）| "url"（自定义在线音源）| "netease"（网易云在线曲库）
    pub kind: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub nid: Option<i64>,
    #[serde(default)]
    pub qid: Option<String>,
    /// 酷狗曲目 hash（酷狗在线曲库）
    #[serde(default)]
    pub kgid: Option<String>,
    /// 播放音质描述（如 "320kbps" / "FLAC"），来自取链接响应
    #[serde(default)]
    pub quality: Option<String>,
    /// LX 音源身份（仅当曲目来自 LX 导入音源时携带，用于回查歌词）：
    /// 音源 id / 平台代码（wy/tx/kg…）/ 歌曲 id。内置平台回退播放时不带。
    #[serde(default)]
    pub lx_source_id: Option<i64>,
    #[serde(default)]
    pub lx_platform: Option<String>,
    #[serde(default)]
    pub lx_song_id: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayState {
    #[serde(flatten)]
    pub info: TrackInfo,
    pub playing: bool,
    /// 单调递增的"开播代次"：start() 每次 +1；前端据此区分
    /// "换曲开播"（进度归零）与"暂停/恢复"（保留进度）
    #[serde(default)]
    pub seq: u64,
}

/// 播放状态快照（含进度）：WebView 挂起恢复后前端主动拉取，
/// 作为恢复窗口期事件推送可能丢失时的权威同步手段
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayStateSnapshot {
    #[serde(flatten)]
    pub state: PlayState,
    pub pos: u64,
}

/// 输出设备信息（前端下拉用）
#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OutputDeviceInfo {
    /// cpal 设备名（唯一标识）
    pub name: String,
    /// 是否当前系统默认输出设备
    pub is_default: bool,
}

/// 可放进两种后端的已类型擦除音源（rodio::Sink 收 Box，ExclusiveSink 持有 Box）
struct BoxedSrc(Box<dyn Source<Item = f32> + Send>);

impl Iterator for BoxedSrc {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        self.0.next()
    }
}

impl Source for BoxedSrc {
    fn current_frame_len(&self) -> Option<usize> {
        self.0.current_frame_len()
    }
    fn sample_rate(&self) -> u32 {
        self.0.sample_rate()
    }
    fn channels(&self) -> u16 {
        self.0.channels()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.0.total_duration()
    }
    fn try_seek(&mut self, to: Duration) -> Result<(), rodio::source::SeekError> {
        self.0.try_seek(to)
    }
}

/// 播放后端：共享模式（rodio/cpal，经系统混音器）或独占模式（WASAPI 直连声卡）。
/// 两者接口刻意对齐，播放/暂停/定位/音量等逻辑不分叉。
enum Output {
    Shared(Sink),
    Exclusive(crate::exclusive::ExclusiveSink),
}

impl Output {
    fn append(&mut self, src: BoxedSrc) {
        match self {
            Output::Shared(sink) => sink.append(src),
            Output::Exclusive(ex) => ex.append(Box::new(src)),
        }
    }
    fn play(&mut self) {
        match self {
            Output::Shared(s) => s.play(),
            Output::Exclusive(e) => e.play(),
        }
    }
    fn pause(&mut self) {
        match self {
            Output::Shared(s) => s.pause(),
            Output::Exclusive(e) => e.pause(),
        }
    }
    fn stop(&mut self) {
        match self {
            Output::Shared(s) => s.stop(),
            Output::Exclusive(e) => e.stop(),
        }
    }
    fn clear(&mut self) {
        match self {
            Output::Shared(s) => s.clear(),
            Output::Exclusive(e) => e.clear(),
        }
    }
    fn empty(&self) -> bool {
        match self {
            Output::Shared(s) => s.empty(),
            Output::Exclusive(e) => e.empty(),
        }
    }
    fn set_volume(&mut self, v: f32) {
        match self {
            Output::Shared(s) => s.set_volume(v),
            Output::Exclusive(e) => e.set_volume(v),
        }
    }
    fn set_speed(&mut self, v: f32) {
        match self {
            Output::Shared(s) => s.set_speed(v),
            Output::Exclusive(e) => e.set_speed(v),
        }
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        match self {
            Output::Shared(s) => s.try_seek(pos),
            Output::Exclusive(e) => e.try_seek(pos),
        }
    }
    fn is_exclusive(&self) -> bool {
        matches!(self, Output::Exclusive(_))
    }
}

pub struct Engine {
    /// 当前输出流 handle（cpal Stream 非 Send，泄漏保活；设备切换时重建）
    out: RwLock<&'static OutputStreamHandle>,
    /// 当前播放后端：共享模式或独占模式
    output: RwLock<Output>,
    /// 用户是否开启了独占模式（下一首生效）
    exclusive_requested: AtomicBool,
    /// 独占模式的实际状态：是否真的在跑独占，以及不可用时的原因
    exclusive_status: RwLock<(bool, Option<String>)>,
    app: AppHandle,
    pub eq: Arc<EqShared>,
    pub pos_ms: Arc<AtomicU64>,
    pub dur_ms: Arc<AtomicU64>,
    pub user_paused: Arc<AtomicBool>,
    pub stopped: Arc<AtomicBool>,
    /// FLAC seek 重建播放链期间置位，避免 monitor 误判"播完"
    pub rebuilding: Arc<AtomicBool>,
    /// start() 换曲瞬间（clear 与 append 之间）置位，避免 monitor 误判"播完"
    pub switching: Arc<AtomicBool>,
    pub current: Arc<RwLock<Option<TrackInfo>>>,
    volume: AtomicU32,
    speed: AtomicU32,
    play_seq: AtomicU64,
    /// 用户当前仍在等待的在线地址，按请求先后从新到旧排列（队首=最新意图）。
    /// 不能只留一个槽：点 A（下载中）→点 B（下载中）→再点 A 时，
    /// 单槽会被 B 覆盖，A 下完后判定"已无人等待"而被静默丢弃。
    want_url: Arc<RwLock<Vec<String>>>,
    /// 正在后台下载的 URL 集合，防止同一 URL 并发下载写坏缓存文件
    downloading: RwLock<HashSet<String>>,
    downloads_dir: PathBuf,
    /// 缓存上限（字节），0 = 不限制；下载完成后超限即 LRU 清理
    cache_limit: AtomicU64,
    smtc: Sender<SmtcMsg>,
    /// 用户指定的输出设备名（None = 跟随系统默认，设备热插拔时自动切换）
    device_pref: RwLock<Option<String>>,
}

impl Engine {
    pub fn new(
        app: AppHandle,
        app_data: &Path,
        volume: f32,
        speed: f32,
        eq: Arc<EqShared>,
        cache_limit: u64,
        smtc: Sender<SmtcMsg>,
    ) -> Result<Self, String> {
        let (handle, sink) = Self::build_output(None)?;
        let downloads_dir = app_data.join("downloads");
        let _ = std::fs::create_dir_all(&downloads_dir);
        // 旧版缓存按 URL 哈希命名（无 net-/qq- 前缀），链接签名变化导致
        // 这些文件永不再命中，启动时清掉以免白占磁盘
        if let Ok(rd) = std::fs::read_dir(&downloads_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                // 必须与 cache_key_for 生成的前缀保持一致，漏一个前缀
                // 就会导致该平台已缓存的曲目每次启动都被当成旧版残留删光
                let recognized = name.starts_with("net-")
                    || name.starts_with("qq-")
                    || name.starts_with("kug-")
                    || name.starts_with("url-");
                if !recognized {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(Self {
            out: RwLock::new(handle),
            output: RwLock::new(Output::Shared(sink)),
            exclusive_requested: AtomicBool::new(false),
            exclusive_status: RwLock::new((false, None)),
            app,
            eq,
            pos_ms: Arc::new(AtomicU64::new(0)),
            dur_ms: Arc::new(AtomicU64::new(0)),
            user_paused: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(true)),
            rebuilding: Arc::new(AtomicBool::new(false)),
            switching: Arc::new(AtomicBool::new(false)),
            current: Arc::new(RwLock::new(None)),
            volume: AtomicU32::new(volume.to_bits()),
            speed: AtomicU32::new(speed.to_bits()),
            play_seq: AtomicU64::new(0),
            want_url: Arc::new(RwLock::new(Vec::new())),
            downloading: RwLock::new(HashSet::new()),
            downloads_dir,
            cache_limit: AtomicU64::new(cache_limit),
            smtc,
            device_pref: RwLock::new(None),
        })
    }

    /// 按设备偏好创建输出流与 Sink；None = 系统默认设备。
    /// cpal Stream 非 Send/Sync：泄漏保活整个进程周期（与旧实现一致），
    /// 设备切换时旧 stream 一起泄漏（仅结构体大小，代价可忽略）。
    fn build_output(
        pref: Option<&str>,
    ) -> Result<(&'static OutputStreamHandle, Sink), String> {
        let host = rodio::cpal::default_host();
        let device = match pref {
            Some(name) => host
                .output_devices()
                .map_err(|e| format!("枚举输出设备失败: {e}"))?
                .find(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
                .ok_or_else(|| format!("输出设备「{name}」不存在"))?,
            None => host
                .default_output_device()
                .ok_or("没有可用的音频输出设备")?,
        };
        let (stream, handle) = rodio::OutputStream::try_from_device(&device)
            .map_err(|e| format!("打开输出设备失败: {e}"))?;
        let _leaked: &'static OutputStream = Box::leak(Box::new(stream));
        let sink = rodio::Sink::try_new(&handle).map_err(|e| format!("创建播放通道失败: {e}"))?;
        sink.pause();
        let leaked_handle: &'static OutputStreamHandle = Box::leak(Box::new(handle));
        Ok((leaked_handle, sink))
    }

    /// 当前使用的输出设备名
    pub fn current_device_name(&self) -> String {
        // cpal 无"stream 绑定的设备"查询；按偏好返回，无偏好时取系统默认
        if let Some(name) = self.device_pref.read().as_deref() {
            return name.to_string();
        }
        rodio::cpal::default_host()
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default()
    }

    pub fn device_preference(&self) -> Option<String> {
        self.device_pref.read().clone()
    }

    pub fn set_device_preference(&self, name: Option<&str>) {
        *self.device_pref.write() = name.map(|s| s.to_string());
    }

    /// 切换输出设备：重建输出流与播放后端，当前曲目从进度处续播。
    /// 独占模式下重建为独占后端；打不开则回退共享模式并记下原因。
    pub fn switch_output_device(self: &Arc<Self>, name: Option<&str>) -> Result<(), String> {
        // 重建期间 monitor 会因后端短暂为空误判"播完"，借用 rebuilding 标志屏蔽
        self.rebuilding.store(true, Ordering::Relaxed);
        let result = (|| -> Result<(), String> {
            let info = self.current.read().clone();
            let pos = self.pos_ms.load(Ordering::Relaxed);
            let was_paused = self.user_paused.load(Ordering::Relaxed);
            let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
            let speed = f32::from_bits(self.speed.load(Ordering::Relaxed));

            self.set_device_preference(name);

            // 重建后端：独占优先，不可用则共享
            let want_exclusive = self.exclusive_requested.load(Ordering::Relaxed);
            let src_rate = self.probe_source_rate(info.as_ref());
            let rebuilt: (Output, Option<&'static OutputStreamHandle>) = if want_exclusive {
                match crate::exclusive::ExclusiveSink::open(name, src_rate) {
                    Ok(ex) => (Output::Exclusive(ex), None),
                    Err(e) => {
                        eprintln!("[engine] 独占模式不可用，已回退普通模式: {e}");
                        let (handle, sink) = Self::build_output(name)?;
                        (Output::Shared(sink), Some(handle))
                    }
                }
            } else {
                let (handle, sink) = Self::build_output(name)?;
                (Output::Shared(sink), Some(handle))
            };

            {
                let (out, handle) = rebuilt;
                let mut old = self.output.write();
                old.stop();
                *old = out;
                if let Some(h) = handle {
                    *self.out.write() = h;
                }
            }
            self.sync_exclusive_status();

            if let Some(info) = info {
                // 当前有曲目：从 pos 处重建播放链。
                // path 一律是本地路径（本地曲目或已缓存的在线音源文件）
                if !info.path.is_empty() {
                    match File::open(&info.path) {
                        Ok(file) => {
                            let src = Decoder::new(buffered_reader(file))
                                .map_err(|e| format!("无法解码该音频文件: {e}"))?
                                .convert_samples::<f32>()
                                .skip_duration(Duration::from_millis(pos));
                            let wrapped = EqSource::with_base(
                                src,
                                self.eq.clone(),
                                self.pos_ms.clone(),
                                pos as f64,
                            );
                            let mut out = self.output.write();
                            out.clear();
                            out.append(BoxedSrc(Box::new(wrapped)));
                            out.set_volume(volume);
                            out.set_speed(speed);
                            if was_paused {
                                out.pause();
                            } else {
                                out.play();
                            }
                            self.stopped.store(false, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("[engine] 切换设备后重开音频失败: {e}");
                        }
                    }
                }
            }
            Ok(())
        })();
        self.rebuilding.store(false, Ordering::Relaxed);
        result
    }

    // ---------- 播放 ----------

    /// 登记"用户正在等这个地址"，队首=最新意图，去重并限制长度。
    fn want_push(&self, url: &str) {
        const MAX_WANT: usize = 8;
        let mut w = self.want_url.write();
        w.retain(|u| u != url);
        w.insert(0, url.to_string());
        w.truncate(MAX_WANT);
    }

    /// 该地址是否是用户当前最新意图（队列中排第一）。
    fn want_is_latest(&self, url: &str) -> bool {
        self.want_url.read().first().map(|s| s.as_str()) == Some(url)
    }

    /// 清空等待意图。播出一首即认为用户已得到想要的曲子：
    /// 队列中更早的地址随后下完也不该再抢播。
    fn want_clear(&self) {
        self.want_url.write().clear();
    }

    pub fn play_file(&self, info: TrackInfo) -> Result<(), String> {
        self.want_clear();
        let file = File::open(&info.path).map_err(|e| format!("打开文件失败: {e}"))?;
        let src = Decoder::new(buffered_reader(file))
            .map_err(|e| format!("无法解码该音频文件: {e}"))?
            .convert_samples::<f32>();
        self.start(src, info)
    }

    fn start<S>(&self, src: S, info: TrackInfo) -> Result<(), String>
    where
        S: Source<Item = f32> + Send + 'static,
    {
        let wrapped = EqSource::new(src, self.eq.clone(), self.pos_ms.clone());
        let diag_sr = wrapped.sample_rate();
        let diag_ch = wrapped.channels();
        let diag_dur = wrapped.total_duration();
        self.pos_ms.store(0, Ordering::Relaxed);
        self.dur_ms
            .store(info.duration_ms, Ordering::Relaxed);
        // 换曲瞬间后端短暂为空，置位避免 monitor 采样到 empty 误判"播完"
        self.switching.store(true, Ordering::Relaxed);
        self.ensure_backend(diag_sr);
        {
            let mut out = self.output.write();
            out.clear();
            out.append(BoxedSrc(Box::new(wrapped)));
            out.set_volume(f32::from_bits(self.volume.load(Ordering::Relaxed)));
            out.set_speed(f32::from_bits(self.speed.load(Ordering::Relaxed)));
            out.play();
        }
        self.switching.store(false, Ordering::Relaxed);
        self.user_paused.store(false, Ordering::Relaxed);
        self.stopped.store(false, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        eprintln!(
            "[engine] started: kind={} path={} total_duration={:?} sr={} ch={} exclusive={}",
            info.kind, info.path, diag_dur, diag_sr, diag_ch, self.is_exclusive_active(),
        );
        *self.current.write() = Some(info.clone());
        self.notify_smtc();
        let seq = self.play_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.app.emit(
            "player://state",
            PlayState { playing: true, info, seq },
        );
        Ok(())
    }

    pub fn pause(&self) {
        {
            let mut out = self.output.write();
            if out.empty() {
                return;
            }
            out.pause();
        }
        self.user_paused.store(true, Ordering::Relaxed);
        self.notify_smtc();
        self.emit_state(false);
    }

    pub fn resume(&self) {
        // 曲目已自然播完（托盘隐藏期间"播完"事件可能丢失）：重启当前曲目，
        // 否则播放按钮会永远无响应（早期 return 既不播放也不发状态）
        if self.output.read().empty() {
            let info = self.current.read().clone();
            if let Some(info) = info {
                let _ = self.play_file(info);
            }
            return;
        }
        self.output.write().play();
        self.user_paused.store(false, Ordering::Relaxed);
        self.stopped.store(false, Ordering::Relaxed);
        self.notify_smtc();
        self.emit_state(true);
    }

    pub fn toggle(&self) {
        // 后端已空（停止/自然播完）时"播放"应重启当前曲目而不是走 pause 早退
        if self.user_paused.load(Ordering::Relaxed) || self.output.read().empty() {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn stop(&self) {
        self.output.write().stop();
        self.stopped.store(true, Ordering::Relaxed);
        self.user_paused.store(false, Ordering::Relaxed);
        self.pos_ms.store(0, Ordering::Relaxed);
        self.notify_smtc();
        self.emit_state(false);
    }

    pub fn seek(&self, ms: u64) -> Result<(), String> {
        if self.output.read().empty() {
            return Err("当前没有正在播放的曲目".into());
        }
        // FLAC：解码器不支持 seek，失败的 try_seek 还会重置解码器状态
        // （表现为进度先跳回开头再跳目标），直接走重建路径
        let info_opt = self.current.read().clone();
        let is_flac = info_opt
            .as_ref()
            .map(|i| i.path.to_lowercase().ends_with(".flac"))
            .unwrap_or(false);
        if is_flac {
            let info = info_opt.ok_or("当前没有正在播放的曲目")?;
            self.rebuilding.store(true, Ordering::Relaxed);
            let r = self.rebuild_at(&info, ms);
            self.rebuilding.store(false, Ordering::Relaxed);
            return r;
        }
        drop(info_opt);
        // 常规 seek；MP3 边界位置偶发失败时回退 300ms 重试
        let mut out = self.output.write();
        match out.try_seek(Duration::from_millis(ms)) {
            Ok(()) => Ok(()),
            Err(first) => {
                let back = ms.saturating_sub(300);
                match out.try_seek(Duration::from_millis(back)) {
                    Ok(()) => Ok(()),
                    Err(_) => Err(format!("定位失败: {first}")),
                }
            }
        }
    }
    /// FLAC 专用：重开文件并丢弃到目标时长，重建播放链
    fn rebuild_at(&self, info: &TrackInfo, ms: u64) -> Result<(), String> {
        let file = std::fs::File::open(&info.path).map_err(|e| format!("重开文件失败: {e}"))?;
        let src = Decoder::new(buffered_reader(file))
            .map_err(|e| format!("重新解码失败: {e}"))?
            .convert_samples::<f32>()
            .skip_duration(Duration::from_millis(ms));
        let wrapped =
            EqSource::with_base(src, self.eq.clone(), self.pos_ms.clone(), ms as f64);
        self.pos_ms.store(ms, Ordering::Relaxed);
        self.dur_ms.store(info.duration_ms, Ordering::Relaxed);
        let sr = wrapped.sample_rate();
        self.ensure_backend(sr);
        let was_paused = self.user_paused.load(Ordering::Relaxed);
        {
            let mut out = self.output.write();
            out.clear();
            out.append(BoxedSrc(Box::new(wrapped)));
            out.set_volume(f32::from_bits(self.volume.load(Ordering::Relaxed)));
            out.set_speed(f32::from_bits(self.speed.load(Ordering::Relaxed)));
            if was_paused {
                out.pause();
            } else {
                out.play();
            }
        }
        Ok(())
    }

    // ---------- 独占模式 ----------

    /// 用当前曲目的文件探测源采样率；探测不出就退回 48k（多数设备都认）
    fn probe_source_rate(&self, info: Option<&TrackInfo>) -> u32 {
        if let Some(i) = info {
            if !i.path.is_empty() {
                if let Ok(file) = File::open(&i.path) {
                    if let Ok(dec) = Decoder::new(buffered_reader(file)) {
                        let sr = dec.sample_rate();
                        if (8000..=192000).contains(&sr) {
                            return sr;
                        }
                    }
                }
            }
        }
        48000
    }

    /// 每首开播前确认后端与用户意图一致。
    /// 独占开启 → 尝试打开独占，失败就回退共享并记下原因（下一首仍会重试）。
    fn ensure_backend(&self, src_rate: u32) {
        let want = self.exclusive_requested.load(Ordering::Relaxed);
        if want == self.output.read().is_exclusive() {
            return;
        }
        let pref = self.device_pref.read().clone();
        if want {
            match crate::exclusive::ExclusiveSink::open(pref.as_deref(), src_rate) {
                Ok(ex) => {
                    let mut out = self.output.write();
                    out.stop();
                    *out = Output::Exclusive(ex);
                }
                Err(e) => {
                    // 打不开独占：回退共享模式，原因交给界面提示
                    eprintln!("[engine] 独占模式不可用，已回退普通模式: {e}");
                    let (handle, sink) = match Self::build_output(pref.as_deref()) {
                        Ok(v) => v,
                        Err(e2) => {
                            eprintln!("[engine] 共享模式也打不开: {e2}");
                            return;
                        }
                    };
                    let mut out = self.output.write();
                    out.stop();
                    *out = Output::Shared(sink);
                    *self.out.write() = handle;
                }
            }
        } else {
            let (handle, sink) = match Self::build_output(pref.as_deref()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[engine] 切回普通模式失败: {e}");
                    return;
                }
            };
            let mut out = self.output.write();
            out.stop();
            *out = Output::Shared(sink);
            *self.out.write() = handle;
        }
        self.sync_exclusive_status();
    }

    /// 把"是否真的在跑独占 + 不可用原因"同步到共享状态，
    /// 独占渲染线程被驱动掐断时也能及时反映出来
    fn sync_exclusive_status(&self) {
        let mut st = self.exclusive_status.write();
        if let Output::Exclusive(ex) = &*self.output.read() {
            *st = (true, ex.thread_error());
        } else {
            *st = (false, None);
        }
    }

    /// 独占模式开关。
    /// - 开启：记下意图，**下一首**生效（规格如此）
    /// - 关闭：立刻把设备交还系统，从当前进度切回普通模式
    pub fn set_exclusive(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        let was = self.exclusive_requested.swap(enabled, Ordering::SeqCst);
        if was == enabled {
            return Ok(());
        }
        if enabled {
            *self.exclusive_status.write() = (false, None);
            return Ok(());
        }
        // 关闭：立即重建为共享后端（会把设备交还系统，其它应用音频恢复）
        self.switch_output_device(self.device_pref.read().as_deref())
    }

    pub fn exclusive_requested(&self) -> bool {
        self.exclusive_requested.load(Ordering::SeqCst)
    }

    pub fn is_exclusive_active(&self) -> bool {
        self.output.read().is_exclusive()
    }

    /// 独占模式状态给界面用：{ enabled, active, reason, device, rate }
    pub fn exclusive_info(&self) -> serde_json::Value {
        let (active, reason) = self.exclusive_status.read().clone();
        let (device, rate) = {
            let out = self.output.read();
            match &*out {
                Output::Exclusive(ex) => {
                    (Some(ex.device_name().to_string()), Some(ex.device_rate()))
                }
                _ => (None, None),
            }
        };
        let active = active && self.output.read().is_exclusive();
        serde_json::json!({
            "enabled": self.exclusive_requested(),
            "active": active,
            "reason": if active { None } else { reason },
            "device": device,
            "rate": rate,
        })
    }

    // ---------- 音量 / 速度 / 均衡器 ----------

    pub fn set_volume(&self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        self.volume.store(v.to_bits(), Ordering::Relaxed);
        self.output.write().set_volume(v);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn set_speed(&self, v: f32) {
        let v = v.clamp(0.5, 2.0);
        self.speed.store(v.to_bits(), Ordering::Relaxed);
        self.output.write().set_speed(v);
    }

    pub fn speed(&self) -> f32 {
        f32::from_bits(self.speed.load(Ordering::Relaxed))
    }

    // ---------- 状态查询 ----------

    pub fn is_active(&self) -> bool {
        !self.output.read().empty()
    }

    fn emit_state(&self, playing: bool) {
        if let Some(info) = self.current.read().clone() {
            let seq = self.play_seq.load(Ordering::Relaxed);
            let _ = self
                .app
                .emit("player://state", PlayState { playing, info, seq });
        }
    }

    /// WebView 挂起恢复后补发当前播放状态与进度（挂起期间发往前端的事件被丢弃）
    pub fn resync_ui(&self) {
        let playing =
            !self.user_paused.load(Ordering::Relaxed) && !self.output.read().empty();
        self.emit_state(playing);
        // 进度帧只在真实播放中补发：引擎空闲时补发 pos(0,0) 会触发前端
        // pos 事件的“playing 自愈”，托盘往返后按钮凭空变成“播放中”
        if playing {
            let _ = self.app.emit(
                "player://pos",
                serde_json::json!({
                    "pos": self.pos_ms.load(Ordering::Relaxed),
                    "dur": self.dur_ms.load(Ordering::Relaxed),
                }),
            );
        }
    }

    /// 播放状态快照（引擎空闲但播过歌时也返回，playing=false）
    pub fn snapshot(&self) -> Option<PlayStateSnapshot> {
        let info = self.current.read().clone()?;
        let playing = !self.user_paused.load(Ordering::Relaxed) && !self.output.read().empty();
        Some(PlayStateSnapshot {
            state: PlayState {
                info,
                playing,
                seq: self.play_seq.load(Ordering::Relaxed),
            },
            pos: self.pos_ms.load(Ordering::Relaxed),
        })
    }

    fn notify_smtc(&self) {
        let info = self.current.read().clone();
        let playing =
            !self.user_paused.load(Ordering::Relaxed) && !self.output.read().empty();
        let _ = self.smtc.send(SmtcMsg::Update {
            info,
            playing,
            pos_ms: self.pos_ms.load(Ordering::Relaxed),
        });
    }

/// 仅刷新系统媒体浮窗进度（播放中由 monitor 周期调用，不重复设置元数据）
    pub fn notify_smtc_pos(&self) {
        if self.current.read().is_none() {
            return;
        }
        let playing =
            !self.user_paused.load(Ordering::Relaxed) && !self.output.read().empty();
        let _ = self.smtc.send(SmtcMsg::Position {
            playing,
            pos_ms: self.pos_ms.load(Ordering::Relaxed),
        });
    }

    // ---------- 输出设备 ----------

    /// 枚举输出设备（含"当前默认"标记）
    pub fn list_output_devices(&self) -> Vec<OutputDeviceInfo> {
        let host = rodio::cpal::default_host();
        let default_name = host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        let mut out = Vec::new();
        if let Ok(devices) = host.output_devices() {
            for d in devices {
                if let Ok(name) = d.name() {
                    out.push(OutputDeviceInfo {
                        is_default: name == default_name,
                        name,
                    });
                }
            }
        }
        out
    }

    // ---------- 在线音源 ----------

    /// 稳定的缓存键：网易云/QQ 的 CDN 直链每次请求都带新的签名参数，
    /// 按 URL 哈希命名会导致同一首歌每次播放都重新下载；
    /// 因此用歌曲 ID + 实际音质做键（同一首歌同一音质只缓存一份）。
    /// 返回 (缓存键, 扩展名)。ext 用于从 URL 提前确定文件扩展名。
    fn cache_key_for(&self, url: &str, info: &TrackInfo) -> (String, String) {
        let ext = url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .map(|e| e.to_lowercase())
            .filter(|e| {
                matches!(
                    e.as_str(),
                    "mp3" | "flac" | "wav" | "ogg" | "oga" | "m4a" | "aac" | "mp4" | "m4b"
                )
            })
            .unwrap_or_else(|| "bin".into());
        // 实际音质（FLAC 按无损档），落到缓存键里：换音质后不命中旧缓存
        let q = info
            .quality
            .as_deref()
            .map(|q| if q.eq_ignore_ascii_case("flac") { "flac".to_string() } else { q.replace([' ', 'k'], "") })
            .unwrap_or_else(|| "d".into());
        let key = match (info.kind.as_str(), info.nid, info.qid.as_deref()) {
            ("netease", Some(nid), _) => format!("net-{nid}-{q}"),
            ("qq", _, Some(qid)) => format!("qq-{qid}-{q}"),
            ("kugou", _, Some(kgid)) => format!("kug-{kgid}-{q}"),
            // 自定义在线音源没有稳定 ID，仍按 URL 哈希
            _ => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                url.hash(&mut h);
                format!("url-{h:016x}-{q}", h = h.finish())
            }
        };
        (key, ext)
    }

    fn cache_path_for(&self, key: &str, ext: &str) -> PathBuf {
        self.downloads_dir.join(format!("{key}.{ext}"))
    }

    /// 播放在线音源：有缓存直接播放，否则后台下载（带进度事件）完成后自动播放。
    /// 下载完成后按缓存上限做 LRU 清理。
    pub fn play_url(self: &Arc<Self>, url: String, info: TrackInfo) -> Result<(), String> {
        let url = url.trim().to_string();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("音源地址必须以 http:// 或 https:// 开头".into());
        }
        if url.contains(".m3u8") {
            return Err("暂不支持 m3u8/HLS 流，请使用音频文件直链".into());
        }
        let (key, ext) = self.cache_key_for(&url, &info);
        let cache = self.cache_path_for(&key, &ext);
        if cache.exists() && cache.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            // 命中缓存：更新访问时间（LRU 依据），当前曲目直接播放
            let _ = filetime::set_file_mtime(
                &cache,
                filetime::FileTime::from_system_time(std::time::SystemTime::now()),
            );
            let mut info = info;
            info.path = cache.to_string_lossy().into_owned();
            if info.duration_ms == 0 {
                info.duration_ms = probe_duration(&cache);
            }
            self.want_clear();
            // 解码失败说明缓存内容不是有效音频（例如误把网页/接口响应存成了缓存）：
            // 删除坏缓存，否则后续每次播放都会命中同一份坏文件
            if let Err(e) = self.play_file(info) {
                let _ = std::fs::remove_file(&cache);
                return Err(e);
            }
            return Ok(());
        }
        self.want_push(&url);
        // 同一缓存键已有下载在进行：只登记意图后返回，复用进行中的下载，
        // 避免两个线程同时写同一个 .part 缓存文件导致内容损坏
        {
            let mut dl = self.downloading.write();
            if dl.contains(&key) {
                return Ok(());
            }
            dl.insert(key.clone());
        }
        let engine = Arc::clone(self);
        let app = self.app.clone();
        std::thread::spawn(move || {
            let result = download_to(&app, &url, &cache);
            engine.downloading.write().remove(&key);
            match result {
                Err(e) => {
                    let _ = app.emit(
                        "download://progress",
                        serde_json::json!({ "url": url, "done": true, "error": e }),
                    );
                    // 该地址仍是最新意图时清除，便于用户下次点击重新发起下载
                    if engine.want_is_latest(&url) {
                        engine.want_clear();
                    }
                }
                Ok(()) => {
                    // 仅当该地址仍是用户最新意图时才自动开播。
                    // 判定用"是否队首"而不是"是否存在"：点 A 再点 B 后，
                    // A 先下完时排在队尾，不该抢在 B 前面播。
                    let still_wanted = engine.want_is_latest(&url);
                    if still_wanted {
                        let mut info = info;
                        info.path = cache.to_string_lossy().into_owned();
                        if info.duration_ms == 0 {
                            info.duration_ms = probe_duration(&cache);
                        }
                        // 下载成功但解码失败 = 内容不是有效音频（多半是网页/接口响应），
                        // 删除坏缓存并上报错误，避免坏文件常驻缓存被反复命中
                        if let Err(e) = engine.play_file(info) {
                            let _ = std::fs::remove_file(&cache);
                            let _ = app.emit(
                                "download://progress",
                                serde_json::json!({ "url": url, "done": true, "error": e }),
                            );
                        }
                    } else {
                        // 已被更新的意图取代：不播，但缓存保留，下次点它可直接命中。
                        // 这里不 emit done —— 前端进度条按"有没有 done"清除，
                        // 对别人的 URL 发 done 会把当前正在显示的下载条误清掉。
                    }
                    // 下载成功后按上限清理（跳过正在播放/下载中的文件）
                    engine.evict_cache();
                }
            }
        });
        Ok(())
    }

    // ---------- 缓存管理 ----------

    /// 缓存目录内所有完整缓存文件（含大小与最后访问时间），按新旧降序
    fn cache_entries(&self) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.downloads_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                if p.extension().and_then(|x| x.to_str()) == Some("part") {
                    continue;
                }
                let Ok(meta) = e.metadata() else { continue };
                let mtime = meta
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                out.push((p, meta.len(), mtime));
            }
        }
        out.sort_by(|a, b| b.2.cmp(&a.2)); // 新 -> 旧
        out
    }

    /// 当前缓存占用（字节）与文件数
    pub fn cache_usage(&self) -> (u64, u32) {
        let mut total = 0u64;
        let mut count = 0u32;
        for (_, len, _) in self.cache_entries() {
            total += len;
            count += 1;
        }
        (total, count)
    }

    /// 强制清理全部缓存；正在播放的文件跳过（Windows 上删除会失败）。
    /// 返回删除的文件数。
    pub fn clear_cache(&self) -> u32 {
        let playing = self
            .current
            .read()
            .as_ref()
            .map(|c| c.path.clone())
            .unwrap_or_default();
        let mut n = 0u32;
        for (p, _, _) in self.cache_entries() {
            if !playing.is_empty() && p == PathBuf::from(&playing) {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// 缓存上限（字节），0 = 不限制
    pub fn cache_limit(&self) -> u64 {
        self.cache_limit.load(Ordering::Relaxed)
    }

    pub fn set_cache_limit(&self, bytes: u64) {
        self.cache_limit.store(bytes, Ordering::Relaxed);
        self.evict_cache();
    }

    /// LRU 清理：超过上限时从最旧开始删，直到回到上限内。
    /// 跳过正在播放的文件和 .part 下载中间文件（后者不占上限，由下载流程自管）。
    fn evict_cache(&self) {
        let limit = self.cache_limit();
        if limit == 0 {
            return;
        }
        let playing = self
            .current
            .read()
            .as_ref()
            .map(|c| c.path.clone())
            .unwrap_or_default();
        let entries = self.cache_entries();
        let mut total = 0u64;
        for (_, len, _) in &entries {
            total += len;
        }
        if total <= limit {
            return;
        }
        for (p, len, _) in entries.iter().rev() {
            if total <= limit {
                break;
            }
            if !playing.is_empty() && *p == PathBuf::from(&playing) {
                continue;
            }
            if std::fs::remove_file(p).is_ok() {
                total = total.saturating_sub(*len);
            }
        }
    }
}

fn probe_duration(path: &Path) -> u64 {
    lofty::read_from_path(path)
        .ok()
        .map(|t| t.properties().duration().as_millis() as u64)
        .unwrap_or(0)
}

/// start() 尾部的公共播放准备已在 Output 上内联（共享/独占两套后端都要设）

/// 下载 URL 到本地缓存文件，通过 download://progress 事件回报进度
fn download_to(app: &AppHandle, url: &str, dest: &Path) -> Result<(), String> {
    let part = dest.with_extension("part");
    // 连接与读取分段超时：整体超时会在大文件（FLAC 等几十 MB）下载中途掐断连接
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("下载音源失败: {e}"))?;
    // Content-Type 预检：网页/JSON/接口响应必然无法解码，提前给出可操作的提示
    // （否则 symphonia 只会报模糊的 "Unrecognized format"）
    let ctype = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
    if ctype.contains("text/html")
        || ctype.contains("application/xhtml")
        || ctype.contains("application/json")
    {
        return Err(format!(
            "该地址返回的是网页/接口响应（{ctype}）而非音频文件：在线音源只支持音频直链（如 https://…/song.mp3）"
        ));
    }
    let total: u64 = resp
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let title_hint = resp
        .header("content-disposition")
        .and_then(|v| {
            v.split(';')
                .rev()
                .find_map(|seg| seg.trim().strip_prefix("filename="))
                .map(|s| s.trim_matches('"').to_string())
        })
        .unwrap_or_default();

    let mut file = File::create(&part).map_err(|e| format!("创建缓存文件失败: {e}"))?;
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    // 首块嗅探：Content-Type 不准但实际返回 HTML 的服务器（自建网盘/反代很常见）
    let n0 = reader
        .read(&mut buf)
        .map_err(|e| format!("下载数据流中断: {e}"))?;
    {
        let head = &buf[..n0];
        let starts = |sig: &[u8]| {
            head.iter()
                .position(|&b| b != 0xEF && b != 0xBB && b != 0xBF && !b.is_ascii_whitespace())
                .map(|i| head[i..].len() >= sig.len() && head[i..i + sig.len()].eq_ignore_ascii_case(sig))
                .unwrap_or(false)
        };
        if n0 > 0 && (starts(b"<!doctype") || starts(b"<html")) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(
                "该地址返回的是网页（HTML）而非音频文件：在线音源只支持音频文件直链（如 https://…/song.mp3），不支持网站首页或 API 地址".into(),
            );
        }
    }
    if n0 > 0 {
        file.write_all(&buf[..n0])
            .map_err(|e| format!("写入缓存失败: {e}"))?;
        received += n0 as u64;
    }
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载数据流中断: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入缓存失败: {e}"))?;
        received += n as u64;
        if last_emit.elapsed() >= Duration::from_millis(300) {
            // WebView 挂起（托盘隐藏）时跳过进度推送：避免反复唤醒渲染进程。
            // 恢复后前端 webview://resumed 兜底复位下载条
            let suspended = app
                .try_state::<crate::AppState>()
                .map(|st| st.webview_suspended.load(Ordering::SeqCst))
                .unwrap_or(false);
            if !suspended {
                last_emit = std::time::Instant::now();
                let pct = if total > 0 {
                    (received as f64 / total as f64 * 100.0) as u64
                } else {
                    0
                };
                let _ = app.emit(
                    "download://progress",
                    serde_json::json!({ "url": url, "received": received, "total": total, "pct": pct, "done": false }),
                );
            }
        }
    }
    drop(file);
    std::fs::rename(&part, dest).map_err(|e| format!("缓存文件重命名失败: {e}"))?;
    let _ = app.emit(
        "download://progress",
        serde_json::json!({ "url": url, "received": received, "total": if total == 0 { received } else { total }, "pct": 100, "done": true }),
    );
    if !title_hint.is_empty() {
        let st = app.state::<crate::AppState>();
        let conn = st.db.lock();
        if let Some(row) = db_lookup_source(&conn, url) {
            if row.1.is_empty() {
                crate::db::update_source_title(&conn, row.0, &title_hint);
            }
        }
    }
    Ok(())
}

fn db_lookup_source(
    conn: &rusqlite::Connection,
    url: &str,
) -> Option<(i64, String)> {
    conn.query_row(
        "SELECT id, title FROM sources WHERE url = ?1",
        rusqlite::params![url],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}
