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
///
/// 独占侧移植自上游 RustMusic 的 `wasapi_out.rs`，采用**会话模型**：由
/// `spawn_exclusive_session` 派生的线程持有采样源，引擎这边只拿一个
/// `ExclusiveCtl` 控制句柄（active/paused/stop/exited）。音量与倍速通过
/// 与引擎共享的 `Arc<AtomicU32>` 传给会话，因此独占下也能实时调。
enum Output {
    Shared(Sink),
    Exclusive(crate::wasapi_out::ExclusiveCtl),
}

impl Output {
    /// 送入一首新曲目。独占侧会终止旧会话并以新源重启；
    /// 初始化失败返回 Err，此时源已被会话线程取走，调用方需自行重建。
    fn append(&mut self, src: BoxedSrc) -> Result<(), String> {
        match self {
            Output::Shared(sink) => {
                sink.append(src);
                Ok(())
            }
            Output::Exclusive(_) => unreachable!("独占会话由 start_exclusive 启动，不走 append"),
        }
    }
    fn play(&mut self) {
        match self {
            Output::Shared(s) => s.play(),
            Output::Exclusive(c) => c.paused.store(false, Ordering::Relaxed),
        }
    }
    fn pause(&mut self) {
        match self {
            Output::Shared(s) => s.pause(),
            Output::Exclusive(c) => c.paused.store(true, Ordering::Relaxed),
        }
    }
    /// 终止播放。独占侧必须等待会话线程真正退出（设备交还系统），
    /// 否则紧接着重建的共享流打不开，表现为"关独占后无声"。
    fn stop(&mut self) {
        match self {
            Output::Shared(s) => s.stop(),
            Output::Exclusive(c) => {
                crate::wasapi_out::wait_session_exit(c, EXCLUSIVE_EXIT_TIMEOUT_MS);
            }
        }
    }
    fn clear(&mut self) {
        match self {
            Output::Shared(s) => s.clear(),
            Output::Exclusive(c) => {
                crate::wasapi_out::wait_session_exit(c, EXCLUSIVE_EXIT_TIMEOUT_MS);
            }
        }
    }
    fn empty(&self) -> bool {
        match self {
            Output::Shared(s) => s.empty(),
            // 会话线程已退出 = 已经没有采样源在跑，必须当"空"处理，
            // 否则 monitor 会一直以为还在播放，进度条也停在原地。
            Output::Exclusive(c) => {
                !c.active.load(Ordering::Relaxed) || c.exited.load(Ordering::Relaxed)
            }
        }
    }
    /// 音量/倍速：共享模式直接作用于 rodio Sink；独占模式由会话线程
    /// 每缓冲块读取引擎共享的原子量（写入动作已在 Engine::set_volume /
    /// set_speed 完成，这里无需再做）。
    ///
    /// 注意：这里**必须**对 Shared 分支生效。历史上整段是空实现，只考虑
    /// 了独占，导致共享模式下音量/静音/倍速全部无效、声音永远 100%。
    fn set_volume(&mut self, v: f32) {
        if let Output::Shared(s) = self {
            s.set_volume(v);
        }
    }
    fn set_speed(&mut self, v: f32) {
        if let Output::Shared(s) = self {
            s.set_speed(v);
        }
    }
    /// 独占会话不支持原地定位（源已被线程消费），返回错误让引擎走重建路径。
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        match self {
            Output::Shared(s) => s.try_seek(pos),
            Output::Exclusive(_) => Err(rodio::source::SeekError::Other(Box::new(
                std::io::Error::other("独占模式不支持原地定位"),
            ))),
        }
    }
    /// 独占后端是否真的还在跑。
    ///
    /// 必须同时看 `exited`：设备被拔出/被系统抢占时，会话线程会自行结束，
    /// 但 `Output` 里仍然是 `Exclusive` 变体。此时若仍报 true，
    /// start_backend 会一直尝试"重建共享流"（而设备其实已经交还），
    /// 设置页也会显示一个并不存在的独占会话。
    fn is_exclusive(&self) -> bool {
        match self {
            Output::Shared(_) => false,
            Output::Exclusive(c) => {
                c.active.load(Ordering::Relaxed) && !c.exited.load(Ordering::Relaxed)
            }
        }
    }

    /// 当前变体是否独占（不管会话死活）。
    ///
    /// 与 is_exclusive 的区别：会话线程已退出时变体仍是 Exclusive，
    /// 直接 append/clear 会撞 unreachable!()。所有"是否切回共享后端"
    /// 的判定必须用这里，UI 展示才用 is_exclusive。
    fn is_exclusive_variant(&self) -> bool {
        matches!(self, Output::Exclusive(_))
    }
}

/// 等待独占会话线程退出的上限。设备被独占客户端占用期间新的共享流打不开，
/// 所以重建共享后端前必须确认旧会话已真正释放设备。
const EXCLUSIVE_EXIT_TIMEOUT_MS: u64 = 1500;

/// 等待独占会话完成设备初始化的上限。这段等待发生在 async 命令的工作线程上，
/// 驱动无响应时必须有硬上限，否则整个命令派发会被拖住（表现为界面无响应）。
const EXCLUSIVE_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// 独占模式在 `Engine::output.write()` 保护下被释放，
/// 渲染线程此时可能正阻塞在设备驱动调用上（蓝牙耳机休眠、USB DAC 唤醒中）。
/// 独占打开失败后进入这段时间的冷却，不再每首歌都重试一次。
const EXCLUSIVE_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

pub struct Engine {
    /// 当前输出流 handle（cpal Stream 非 Send，泄漏保活；设备切换时重建）
    out: RwLock<&'static OutputStreamHandle>,
    /// 当前播放后端：共享模式或独占模式
    output: RwLock<Output>,
    /// 用户是否开启了独占模式（下一首生效）
    exclusive_requested: AtomicBool,
    /// 独占模式的实际状态：是否真的在跑独占，以及不可用时的原因
    exclusive_status: RwLock<(bool, Option<String>)>,
    /// 独占打开失败后的冷却截止时刻。设备不支持时若每首歌都重试，
    /// 每次都要付出一次设备枚举 + 超时等待，播放会被反复拖住。
    exclusive_retry_at: RwLock<Option<std::time::Instant>>,
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
    // 独占会话线程直接读这两个原子量（与引擎共享 Arc），所以独占模式下
    // 音量与倍速同样能实时生效，无需重建会话。
    volume: Arc<AtomicU32>,
    speed: Arc<AtomicU32>,
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
    /// 输出设备处于故障状态（打开失败/驱动无响应）。置位后由
    /// device_watcher 每 2 秒尝试恢复，恢复成功才清除。
    device_broken: AtomicBool,
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
            exclusive_retry_at: RwLock::new(None),
            app,
            eq,
            pos_ms: Arc::new(AtomicU64::new(0)),
            dur_ms: Arc::new(AtomicU64::new(0)),
            user_paused: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(true)),
            rebuilding: Arc::new(AtomicBool::new(false)),
            switching: Arc::new(AtomicBool::new(false)),
            current: Arc::new(RwLock::new(None)),
            volume: Arc::new(AtomicU32::new(volume.to_bits())),
            speed: Arc::new(AtomicU32::new(speed.to_bits())),
            play_seq: AtomicU64::new(0),
            want_url: Arc::new(RwLock::new(Vec::new())),
            downloading: RwLock::new(HashSet::new()),
            downloads_dir,
            cache_limit: AtomicU64::new(cache_limit),
            smtc,
            device_pref: RwLock::new(None),
            device_broken: AtomicBool::new(false),
        })
    }

    /// 按设备偏好创建输出流与 Sink；None = 系统默认设备。
    /// cpal Stream 非 Send/Sync：泄漏保活整个进程周期（与旧实现一致），
    /// 设备切换时旧 stream 一起泄漏（仅结构体大小，代价可忽略）。
    ///
    /// WASAPI 的设备打开调用（IAudioClient::Initialize / GetMixFormat 等）
    /// **没有内部超时**：声卡驱动无响应时调用会无限阻塞。若直接跑在
    /// async 命令线程上，整个命令永久挂起 —— 用户侧表现为"开关独占后
    /// 界面点了没反应"（v1.1.4 复现：关闭独占后 30 秒不返回）。
    /// 因此把真正的打开动作放到独立线程，5 秒没返回即报超时。
    /// 线程若真卡死在驱动调用里无法回收——泄漏一个线程换取应用不冻结，
    /// 这是代价最小的取舍。
    fn build_output(
        pref: Option<&str>,
    ) -> Result<(&'static OutputStreamHandle, Sink), String> {
        const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let pref = pref.map(|s| s.to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("audio-open".into())
            .spawn(move || {
                let _ = tx.send(Self::build_output_inner(pref.as_deref()));
            })
            .map_err(|e| format!("启动音频设备打开线程失败: {e}"))?;
        match rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("打开输出设备超时：声卡驱动无响应（可尝试重启应用或切换输出设备）".into()),
        }
    }

    /// build_output 的实际动作：枚举设备并创建输出流（跑在独立线程上，
    /// 见 build_output 的注释）。返回前不泄漏任何句柄。
    fn build_output_inner(
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

    /// 打开输出流，带短重试与"偏好设备 → 系统默认"降级。
    ///
    /// 设备刚插上、刚被别的程序独占、或从休眠唤醒时，WASAPI 会以
    /// 0x8889000A(AUDCLNT_E_DEVICE_INVALIDATED) 拒绝打开共享流。这类失败
    /// 往往是瞬时的：重开一次流就成功。此前 build_output 失败即整首放弃，
    /// 而 start() 在 emit 之前就返回，表现为"点了没反应、界面永远未在播放"。
    fn build_output_resilient(
        pref: Option<&str>,
    ) -> Result<(&'static OutputStreamHandle, Sink), String> {
        // 第一次失败后重试两次。间隔取 120/350ms：够设备从"刚被别的程序
        // 抢占"里恢复，又不至于让界面明显卡顿。
        const RETRY_DELAYS_MS: [u64; 2] = [120, 350];
        let mut last_err = String::new();
        for (i, delay) in std::iter::once(0u64).chain(RETRY_DELAYS_MS).enumerate() {
            if delay > 0 {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            match Self::build_output(pref) {
                Ok(v) => {
                    if i > 0 {
                        crate::elog!(
                            "[engine] 输出设备第 {} 次重试后恢复 pref={:?}",
                            i,
                            pref
                        );
                    }
                    return Ok(v);
                }
                Err(e) => {
                    crate::elog!("[engine] 打开输出设备失败(第{}次) pref={:?} err={e}", i + 1, pref);
                    // 驱动无响应（超时）不是瞬时问题，重试只会再等一遍 5 秒
                    // 超时；立即返回，交给 device_watcher 的自动恢复兜底。
                    if e.contains("超时") {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }
        // 用户指定的设备反复打不开：降级到系统默认再试一次。
        // 蓝牙耳机/USB DAC 断开时"记住的设备"会失效，此时跟着系统走
        // 远好过彻底无声。
        if pref.is_some() {
            crate::elog!("[engine] 偏好设备不可用，降级到系统默认设备");
            if let Ok(v) = Self::build_output_resilient(None) {
                return Ok(v);
            }
        }
        Err(format!("{last_err}（已重试并尝试系统默认设备）"))
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
            crate::elog!("[engine] switch_output_device 开始 pref={name:?}");
            let info = self.current.read().clone();
            let pos = self.pos_ms.load(Ordering::Relaxed);
            let was_paused = self.user_paused.load(Ordering::Relaxed);
            let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
            let speed = f32::from_bits(self.speed.load(Ordering::Relaxed));

            // 注意：偏好只在成功打开设备后才落库。提前写入的话，一次
            // 失败就会把偏好永久改成打不开的设备，之后每次播放都去撞它。
            // 独占会话的源由线程独占持有，设备切换时不能"原地搬到新设备"，
            // 统一重建为共享后端并从当前进度续播（独占会在下次开播时重新建立）。
            let (handle, sink) = Self::build_output_resilient(name)?;
            crate::elog!("[engine] switch_output_device 新输出流已建立");
            self.set_device_preference(name);
            {
                let mut old = self.output.write();
                // 先等独占会话真正退出、设备交还系统，再建共享流，否则无声
                old.stop();
                *old = Output::Shared(sink);
                *self.out.write() = handle;
            }
            // old 已释放写锁（块结束），这里只写 exclusive_status
            self.sync_exclusive_status(false);

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
                            out.append(BoxedSrc(Box::new(wrapped)))?;
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
                            crate::elog!("[engine] 切换设备后重开音频失败: {e}");
                        }
                    }
                }
            }
            Ok(())
        })();
        self.rebuilding.store(false, Ordering::Relaxed);
        match &result {
            Ok(()) => {
                self.device_broken.store(false, Ordering::Relaxed);
                crate::elog!("[engine] switch_output_device 完成 pref={name:?}");
            }
            Err(e) => {
                // 置位自愈标志：device_watcher 会周期重试直到设备恢复
                self.device_broken.store(true, Ordering::Relaxed);
                crate::elog!("[engine] switch_output_device 失败 pref={name:?} err={e}");
            }
        }
        result
    }

    /// 输出设备是否处于故障状态（供 device_watcher 自动恢复）
    pub fn device_broken(&self) -> bool {
        self.device_broken.load(Ordering::Relaxed)
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
        self.start(info.path.clone(), info, 0)
    }

    /// 开播一首歌。`skip_ms` 非 0 时从该位置起播（FLAC 重建 / 独占不支持原地定位）。
    ///
    /// 独占会话（wasapi_out）由线程接管采样源，初始化失败时源已不可用，
    /// 因此这里保留 `path` 以便失败后重新解码一次再回退共享模式。
    fn start(&self, path: String, info: TrackInfo, skip_ms: u64) -> Result<(), String> {
        // 统一用 BoxedSrc 作为源类型：skip_duration 包装会改变具体类型，
        // 而失败重建需要一个可多次调用的同签名闭包。
        type AnySrc = EqSource<BoxedSrc>;
        let build = || -> Result<AnySrc, String> {
            let file = File::open(&path).map_err(|e| format!("打开文件失败: {e}"))?;
            let dec = Decoder::new(buffered_reader(file))
                .map_err(|e| format!("无法解码该音频文件: {e}"))?
                .convert_samples::<f32>();
            let inner: BoxedSrc = if skip_ms > 0 {
                BoxedSrc(Box::new(dec.skip_duration(Duration::from_millis(skip_ms))))
            } else {
                BoxedSrc(Box::new(dec))
            };
            Ok(EqSource::new(inner, self.eq.clone(), self.pos_ms.clone()))
        };
        let wrapped = match build() {
            Ok(w) => w,
            Err(e) => {
                // 这一步在下面的埋点之前，历史上失败时完全静默：
                // 日志里只剩"下载完成"而没有任何后续，排查时看不出死在哪。
                crate::elog!("[engine] 解码失败 kind={} path={} err={e}", info.kind, info.path);
                return Err(e);
            }
        };
        let diag_sr = wrapped.sample_rate();
        let diag_ch = wrapped.channels();
        let diag_dur = wrapped.total_duration();
        let _ = diag_dur;
        self.pos_ms.store(skip_ms, Ordering::Relaxed);
        self.dur_ms
            .store(info.duration_ms, Ordering::Relaxed);
        // 换曲瞬间后端短暂为空，置位避免 monitor 采样到 empty 误判"播完"
        self.switching.store(true, Ordering::Relaxed);
        let result = self.start_backend(wrapped, build, diag_sr);
        self.switching.store(false, Ordering::Relaxed);
        if let Err(e) = result {
            crate::elog!(
                "[engine] start 失败 kind={} path={} err={e}",
                info.kind,
                info.path
            );
            // 设备类失败置位自愈标志，交由 device_watcher 周期重试恢复
            if is_device_error(&e) {
                self.device_broken.store(true, Ordering::Relaxed);
            }
            // 失败时后端可能已被 stop/clear 成空，但 current 还指着上一首。
            // 不清掉的话界面会继续显示旧曲目、进度条照走，表现为"点了没反应"。
            // 发 playing=false 让前端把状态归位。
            self.stopped.store(true, Ordering::Relaxed);
            self.user_paused.store(false, Ordering::Relaxed);
            let seq = self.play_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let ps = PlayState {
                playing: false,
                info,
                seq,
            };
            let _ = self.app.emit("player://state", ps);
            return Err(e);
        }
        self.user_paused.store(false, Ordering::Relaxed);
        self.stopped.store(false, Ordering::Relaxed);
        crate::elog!(
            "[engine] started kind={} title={:?} path={} sr={} ch={} dur={}ms exclusive={}",
            info.kind,
            info.title,
            info.path,
            diag_sr,
            diag_ch,
            info.duration_ms,
            self.is_exclusive_active(),
        );
        *self.current.write() = Some(info.clone());
        self.notify_smtc();
        let seq = self.play_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ps = PlayState { playing: true, info, seq };
        let r = self.app.emit("player://state", ps);
        crate::logfile::write(&format!("[engine] emit player://state seq={seq} -> {r:?}"));
        Ok(())
    }

    /// 按当前后端意图开播：独占可用则走独占会话，否则共享模式。
    /// 独占失败会回退共享并重建采样源（源已被会话线程取走）。
    fn start_backend(
        &self,
        wrapped: EqSource<BoxedSrc>,
        rebuild: impl Fn() -> Result<EqSource<BoxedSrc>, String>,
        src_rate: u32,
    ) -> Result<(), String> {
        if self.exclusive_requested.load(Ordering::Relaxed) && !self.exclusive_in_cooldown() {
            // 开新会话前必须先停掉旧后端：独占会话占着设备时，
            // 新会话与共享流都会以 AUDCLNT_E_DEVICE_INVALIDATED(0x8889000A)
            // 打开失败（表现为切歌必失败）。stop() 对独占会等待会话线程
            // 真正退出、设备交还系统。
            self.output.write().stop();
            let pref = self.device_pref.read().clone();
            let params = crate::wasapi_out::ExclusiveParams {
                device_pref: pref.clone(),
                channels: 2,
                src_rate,
                volume_bits: Arc::clone(&self.volume),
                speed_bits: Arc::clone(&self.speed),
            };
            match crate::wasapi_out::spawn_exclusive_session(
                BoxedSrc(Box::new(wrapped)),
                params,
            ) {
                Ok((ctl, rx)) => {
                    // 必须有超时：设备驱动的 COM 调用可能无限期阻塞，
                    // 而这里跑在 async 命令的工作线程上，不能被拖住。
                    match rx.recv_timeout(EXCLUSIVE_OPEN_TIMEOUT) {
                        Ok(Ok(())) => {
                            let mut out = self.output.write();
                            *out = Output::Exclusive(ctl);
                            let active = out.is_exclusive();
                            // 必须先释放写锁：sync_exclusive_status 内部要读
                            // output，写锁未释放时重入读锁会自死锁
                            //（RwLock 写优先，同一线程等自己的写锁释放）。
                            // 历史上这里直接调用，独占首播成功但 start()
                            // 永远不返回 —— 表现为"歌在放但播放器无响应"。
                            drop(out);
                            self.sync_exclusive_status(active);
                            return Ok(());
                        }
                        Ok(Err(e)) => {
                            crate::elog!("[engine] 独占模式不可用，已回退普通模式: {e}");
                            *self.exclusive_retry_at.write() =
                                Some(std::time::Instant::now() + EXCLUSIVE_RETRY_COOLDOWN);
                        }
                        Err(_) => {
                            crate::elog!("[engine] 独占模式初始化超时，已回退普通模式");
                            crate::wasapi_out::wait_session_exit(&ctl, EXCLUSIVE_EXIT_TIMEOUT_MS);
                            *self.exclusive_retry_at.write() =
                                Some(std::time::Instant::now() + EXCLUSIVE_RETRY_COOLDOWN);
                        }
                    }
                }
                Err(e) => {
                    crate::elog!("[engine] 无法启动独占会话: {e}");
                    *self.exclusive_retry_at.write() =
                        Some(std::time::Instant::now() + EXCLUSIVE_RETRY_COOLDOWN);
                }
            }
            // 独占失败：源已被会话取走或从未使用，重新构建一份走共享
            let src = rebuild()?;
            let pref = self.device_pref.read().clone();
            let (handle, sink) = Self::build_output_resilient(pref.as_deref())?;
            let mut out = self.output.write();
            *out = Output::Shared(sink);
            *self.out.write() = handle;
            out.clear();
            out.append(BoxedSrc(Box::new(src)))?;
            out.play();
            let active = out.is_exclusive();
            drop(out);
            self.sync_exclusive_status(active);
            return Ok(());
        }

        // 共享模式：按需把后端切回共享
        // 判定用 is_exclusive_variant：会话即使已退出，变体仍是 Exclusive，
        // 不切回的话下面的 clear/append 会撞 unreachable!()。
        if self.output.read().is_exclusive_variant() {
            // 顺序不能反：旧独占还占着设备时共享流打不开（0x8889000A），
            // 必须先等会话退出、设备交还系统，再建共享流。
            self.output.write().stop();
            let pref = self.device_pref.read().clone();
            let (handle, sink) = Self::build_output_resilient(pref.as_deref())?;
            let mut out = self.output.write();
            *out = Output::Shared(sink);
            *self.out.write() = handle;
            let active = out.is_exclusive();
            drop(out);
            self.sync_exclusive_status(active);
        }
        let mut out = self.output.write();
        out.clear();
        out.append(BoxedSrc(Box::new(wrapped)))?;
        out.set_volume(f32::from_bits(self.volume.load(Ordering::Relaxed)));
        out.set_speed(f32::from_bits(self.speed.load(Ordering::Relaxed)));
        out.play();
        Ok(())
    }

    fn exclusive_in_cooldown(&self) -> bool {
        match *self.exclusive_retry_at.read() {
            Some(until) => std::time::Instant::now() < until,
            None => false,
        }
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
    /// 从指定位置重建播放链（FLAC 定位失败、以及独占模式下的定位都走这里）。
    /// 独占后端无法原地定位（采样源已被会话线程消费），统一重开播放链。
    fn rebuild_at(&self, info: &TrackInfo, ms: u64) -> Result<(), String> {
        self.start(info.path.clone(), info.clone(), ms)
    }

    // ---------- 独占模式 ----------

    /// 把"是否真的在跑独占"同步到共享状态，供设置页展示。
    ///
    /// `active` 由调用方在**释放 output 锁之后**传入：本函数内部只写
    /// exclusive_status，不再读 output。历史上它在持有 output 写锁时被
    /// 调用、内部再取读锁，RwLock 写优先导致同一线程自死锁。
    fn sync_exclusive_status(&self, active: bool) {
        let mut st = self.exclusive_status.write();
        st.0 = active;
        if !active {
            st.1 = None;
        }
    }

    /// 独占模式开关。
    /// - 开启：记下意图，**下一首**生效（规格如此）
    /// - 关闭：立刻把设备交还系统，从当前进度切回普通模式
    pub fn set_exclusive(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        crate::elog!("[engine] set_exclusive({enabled}) 请求");
        let was = self.exclusive_requested.swap(enabled, Ordering::SeqCst);
        if was == enabled {
            crate::elog!("[engine] set_exclusive({enabled}) 状态未变，跳过");
            return Ok(());
        }
        if enabled {
            *self.exclusive_status.write() = (false, None);
            crate::elog!("[engine] set_exclusive(true) 已登记意图，下一首生效");
            return Ok(());
        }
        // 关闭：立即重建为共享后端（会把设备交还系统，其它应用音频恢复）
        let r = self.switch_output_device(self.device_pref.read().as_deref());
        crate::elog!("[engine] set_exclusive(false) 结果: {r:?}");
        r
    }

    pub fn exclusive_requested(&self) -> bool {
        self.exclusive_requested.load(Ordering::SeqCst)
    }

    pub fn is_exclusive_active(&self) -> bool {
        self.output.read().is_exclusive()
    }

    /// 独占模式状态给界面用：{ enabled, active, reason, device, rate }
    /// 独占会话（wasapi_out）不向外暴露协商结果，设备/采样率改用当前偏好设备描述。
    pub fn exclusive_info(&self) -> serde_json::Value {
        let (active, reason) = self.exclusive_status.read().clone();
        let active = active && self.output.read().is_exclusive();
        let device = self
            .device_preference()
            .unwrap_or_else(|| self.current_device_name());
        serde_json::json!({
            "enabled": self.exclusive_requested(),
            "active": active,
            "reason": if active { None } else { reason },
            "device": if active { Some(device) } else { None },
            // 独占会话不向外暴露协商出的采样率
            "rate": Option::<u32>::None,
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
        crate::logfile::write(&format!(
            "[engine] play_url kind={} title={:?} key={key} ext={ext} 缓存命中={}",
            info.kind,
            info.title,
            cache.exists() && cache.metadata().map(|m| m.len() > 0).unwrap_or(false)
        ));
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
            // 删除坏缓存，否则后续每次播放都会命中同一份坏文件。
            // 但设备类失败（打不开输出设备）时文件本身是好的，删掉等于让用户
            // 白白重新下载一遍，且下次仍会失败 —— 这类错误只上报、不删。
            if let Err(e) = self.play_file(info) {
                if is_device_error(&e) {
                    crate::elog!("[engine] 设备错误，保留缓存 key={key} err={e}");
                } else {
                    let _ = std::fs::remove_file(&cache);
                }
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
                    // 下载成功：登记到下载管理（已下载列表）。
                    // 播放触发的缓存下载同样可见，任务 id 用缓存键保证幂等。
                    // 失败时不登记：重试成功后再出现。
                    {
                        let st = app.state::<crate::AppState>();
                        let conn = st.db.lock();
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let size = cache.metadata().map(|m| m.len() as i64).unwrap_or(0);
                        let song_id = info
                            .kgid
                            .clone()
                            .or_else(|| info.nid.map(|v| v.to_string()))
                            .or_else(|| info.qid.clone())
                            .unwrap_or_default();
                        crate::db::upsert_download_task(
                            &conn,
                            &crate::db::DownloadTask {
                                id: format!("cache:{key}"),
                                kind: info.kind.clone(),
                                song_id,
                                media_mid: String::new(),
                                title: info.title.clone(),
                                artist: info.artist.clone(),
                                album: info.album.clone(),
                                cover: info.cover.clone(),
                                size,
                                received: size,
                                status: "done".into(),
                                error: String::new(),
                                file_path: cache.to_string_lossy().into_owned(),
                                created_at: now,
                                finished_at: now,
                            },
                        );
                    }
                    // 仅当该地址仍是用户最新意图时才自动开播。
                    // 判定用"是否队首"而不是"是否存在"：点 A 再点 B 后，
                    // A 先下完时排在队尾，不该抢在 B 前面播。
                    let still_wanted = engine.want_is_latest(&url);
                    crate::logfile::write(&format!(
                        "[engine] 下载完成 key={key} 仍为最新意图={still_wanted}"
                    ));
                    if still_wanted {
                        let mut info = info;
                        info.path = cache.to_string_lossy().into_owned();
                        if info.duration_ms == 0 {
                            info.duration_ms = probe_duration(&cache);
                        }
                        // 下载成功但解码失败 = 内容不是有效音频（多半是网页/接口响应），
                        // 删除坏缓存并上报错误，避免坏文件常驻缓存被反复命中。
                        // 设备类失败则保留：文件是好的，删了下次还得重下。
                        match engine.play_file(info) {
                            Ok(()) => crate::logfile::write(&format!(
                                "[engine] 缓存命中开播成功 key={key}"
                            )),
                            Err(e) => {
                                let device_err = is_device_error(&e);
                                crate::elog!(
                                    "[engine] 缓存解码失败 key={key} 设备错误={device_err} err={e}"
                                );
                                if !device_err {
                                    let _ = std::fs::remove_file(&cache);
                                }
                                let _ = app.emit(
                                    "download://progress",
                                    serde_json::json!({ "url": url, "done": true, "error": e }),
                                );
                            }
                        }
                    } else {
                        // 已被更新的意图取代：不播，但缓存保留，下次点它可直接命中。
                        // 这里不 emit done —— 前端进度条按"有没有 done"清除，
                        // 对别人的 URL 发 done 会把当前正在显示的下载条误清掉。
                        crate::logfile::write(&format!(
                            "[engine] 放弃开播（已被更新的意图取代）key={key}"
                        ));
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

/// 判定一个播放错误是否属于"音频输出设备"问题。
///
/// 这类错误与音源本身无关：文件可能完全正常，只是声卡被别的程序抢占、
/// 刚插上还没就绪、或从休眠唤醒。调用方据此避免删掉已下好的缓存，
/// 前端也据此提示"检查声卡"而不是"该歌曲不可用"。
fn is_device_error(msg: &str) -> bool {
    msg.contains("打开输出设备失败")
        || msg.contains("创建播放通道失败")
        || msg.contains("枚举输出设备失败")
        || msg.contains("没有可用的音频输出设备")
        || msg.contains("输出设备")
        || msg.contains("独占")
        || msg.contains("0x8889")
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

#[cfg(test)]
mod tests {
    use super::is_device_error;

    /// 设备类错误必须与音源类错误区分开：判错会导致已下载好的音频被删除，
    /// 用户不得不重新下载，而重新下载后依然失败（因为问题在声卡）。
    #[test]
    fn device_errors_are_recognized() {
        assert!(is_device_error("打开输出设备失败: A backend-specific error: 0x8889000A"));
        assert!(is_device_error("创建播放通道失败: xxx"));
        assert!(is_device_error("没有可用的音频输出设备"));
        assert!(is_device_error("输出设备「Speakers」不存在"));
        assert!(is_device_error("打开输出设备失败（已重试并尝试系统默认设备）"));
    }

    #[test]
    fn content_errors_are_not_device_errors() {
        // 这些错才允许删缓存
        assert!(!is_device_error("无法解码该音频文件: missing header"));
        assert!(!is_device_error("打开文件失败: 系统找不到指定的文件"));
        assert!(!is_device_error("该歌曲暂无版权"));
        assert!(!is_device_error("下载失败: connection reset"));
    }
}
