//! WASAPI 独占模式输出：绕过 Windows 系统混音器直连声卡。
//!
//! 与共享模式（rodio/cpal）的区别：共享模式下音频经系统混音器、由系统按
//! 设备默认格式重采样并挂上音效处理；独占模式应用自己挑设备支持的格式，
//! 采样率尽量按源文件直通，系统其它应用的音频会被静音（这正是"独占"）。
//!
//! 关键约束：COM 对象（AudioClient / AudioRenderClient）不是 `Send`，必须
//! 完整地留在创建它的那个线程上。所以整个 client 归渲染线程独占，
//! 外部只通过 `Shared` 里的原子量与互斥源通信。
//!
//! 独占不是所有设备都支持：蓝牙耳机、网络音箱、HDMI 等驱动普遍拒绝独占
//! （IAudioClient::Initialize 返回 AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED）。
//! 此时 `ExclusiveSink::open` 返回带原因的 Err，由调用方回退到共享模式并
//! 把原因透给界面。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use rodio::Source;

/// 独占模式不可用的分类，用于给界面一句人话解释
#[derive(Debug, Clone, PartialEq)]
pub enum ExclusiveUnsupported {
    /// 驱动明确拒绝独占（蓝牙/网络音箱等最常见）
    NotAllowed(String),
    /// 设备完全找不到（独占走的是独立的设备枚举路径，名字可能对不上）
    DeviceMissing(String),
    /// COM / 音频端点初始化失败
    Init(String),
    /// 没有任何可用的独占格式
    NoFormat(String),
}

impl std::fmt::Display for ExclusiveUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExclusiveUnsupported::NotAllowed(m) => {
                write!(f, "该设备驱动不支持独占模式：{m}")
            }
            ExclusiveUnsupported::DeviceMissing(m) => write!(f, "找不到输出设备：{m}"),
            ExclusiveUnsupported::Init(m) => write!(f, "独占模式初始化失败：{m}"),
            ExclusiveUnsupported::NoFormat(m) => write!(f, "没有可用的独占输出格式：{m}"),
        }
    }
}

/// 渲染线程与外部共享的状态
struct Shared {
    /// 待播/正在播的解码源（已含 EQ 与进度统计）。独占模式不走 rodio 的
    /// ring buffer，源由渲染线程直接消费。
    src: Mutex<Option<Box<dyn Source<Item = f32> + Send>>>,
    volume: AtomicU32,
    speed: AtomicU32,
    playing: AtomicBool,
    /// 源已读完（自然播完），与 rodio 的 empty() 语义对齐
    eof: AtomicBool,
    /// 显式 stop()，区别于自然播完
    stopped: AtomicBool,
    /// seek 代数：源被定位后自增，渲染线程据此重置重采样相位
    seek_gen: AtomicU64,
    /// 渲染线程退出信号
    shutdown: AtomicBool,
    /// 渲染线程异常退出时的原因，供上层查询
    thread_error: Mutex<Option<String>>,
}

/// 独占模式播放通道。接口刻意与 `rodio::Sink` 对齐，
/// 让引擎层可以按后端二选一而不必分叉播放逻辑。
pub struct ExclusiveSink {
    shared: std::sync::Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    /// 实际协商出的设备采样率，写日志/诊断用
    device_rate: u32,
    device_name: String,
}

impl ExclusiveSink {
    /// 打开独占输出。
    ///
    /// `device` 为 Some 时用指定设备（名字匹配共享模式那边的显示名），
    /// None 表示跟随系统默认设备。
    /// `src_rate` 是源文件采样率——优先按它直通（"采样率按源文件直通"），
    /// 设备不接受时依次退回设备默认格式。
    pub fn open(device: Option<&str>, src_rate: u32) -> Result<Self, ExclusiveUnsupported> {
        render_thread_open(device, src_rate)
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_rate(&self) -> u32 {
        self.device_rate
    }

    /// 渲染线程是否已异常退出（独占被驱动悄悄掐断时会出现）
    pub fn thread_error(&self) -> Option<String> {
        self.shared.thread_error.lock().clone()
    }

    // ---- 与 rodio::Sink 对齐的接口 ----

    pub fn append(&mut self, src: Box<dyn Source<Item = f32> + Send>) {
        let mut guard = self.shared.src.lock();
        *guard = Some(src);
        self.shared.eof.store(false, Ordering::SeqCst);
        self.shared.stopped.store(false, Ordering::SeqCst);
        self.shared.seek_gen.fetch_add(1, Ordering::SeqCst);
    }

    pub fn play(&mut self) {
        self.shared.playing.store(true, Ordering::SeqCst);
    }

    pub fn pause(&mut self) {
        self.shared.playing.store(false, Ordering::SeqCst);
    }

    pub fn stop(&mut self) {
        *self.shared.src.lock() = None;
        self.shared.eof.store(true, Ordering::SeqCst);
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.playing.store(false, Ordering::SeqCst);
    }

    /// 清空待播内容但不复位 stopped（对齐 rodio::Sink::clear 的语义）
    pub fn clear(&mut self) {
        *self.shared.src.lock() = None;
        self.shared.eof.store(true, Ordering::SeqCst);
        self.shared.seek_gen.fetch_add(1, Ordering::SeqCst);
    }

    pub fn empty(&self) -> bool {
        self.shared.eof.load(Ordering::SeqCst) || self.shared.src.lock().is_none()
    }

    pub fn set_volume(&mut self, v: f32) {
        self.shared.volume.store(v.to_bits(), Ordering::SeqCst);
    }

    pub fn set_speed(&mut self, v: f32) {
        self.shared.speed.store(v.to_bits(), Ordering::SeqCst);
    }

    /// 定位。对齐 rodio::Sink::try_seek：成功返回 Ok(())。
    /// FLAC 的解码器不支持 seek，引擎层已先行分流到重建路径。
    pub fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        let mut guard = self.shared.src.lock();
        let Some(src) = guard.as_mut() else {
            return Err(rodio::source::SeekError::Other(Box::new(
                std::io::Error::new(std::io::ErrorKind::NotFound, "没有正在播放的曲目"),
            )));
        };
        src.try_seek(pos)?;
        // 通知渲染线程丢掉重采样器里残留的旧数据
        self.shared.seek_gen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for ExclusiveSink {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // 释放源持有的锁，渲染线程才能从 src.lock() 里退出来
        {
            let mut guard = self.shared.src.lock();
            *guard = None;
        }
        self.shared.eof.store(true, Ordering::SeqCst);
        self.shared.playing.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ---------------- 重采样 ----------------

/// 线性插值重采样器。
///
/// 独占模式要求"源采样率直通"，设备接受源采样率时 ratio 恰为 1（此时
/// 走直通快路径，零重采样损失）；只有设备不接受、或者用户调了倍速时才
/// 真正做插值。倍速沿用 rodio::Sink::set_speed 的行为（会改变音高），
/// 与共享模式保持一致。
struct Resampler {
    ratio: f64,
    /// 落在 [0, 1) 的小数相位：0 = 正好在 cur 上，1 = 正好在 nxt 上
    pos: f64,
    /// 基准输入帧（输出锚点）与它的下一帧，插值为 cur + (nxt - cur) * pos
    cur: Vec<f32>,
    nxt: Vec<f32>,
    /// 是否已经从源里取过第一帧
    primed: bool,
    in_channels: u16,
}

impl Resampler {
    fn new(in_channels: u16) -> Self {
        let ch = in_channels.max(1) as usize;
        Resampler {
            ratio: 1.0,
            pos: 0.0,
            cur: vec![0.0; ch],
            nxt: vec![0.0; ch],
            primed: false,
            in_channels: in_channels.max(1),
        }
    }

    /// ratio = 每产出一个输出采样，需要消耗多少个输入采样
    fn set_ratio(&mut self, ratio: f64) {
        if (ratio - self.ratio).abs() > f64::EPSILON {
            self.ratio = ratio;
        }
    }

    /// 换曲/seek 后作废：下一帧重新从源的当前位置取基准
    fn reset(&mut self) {
        self.pos = 0.0;
        self.primed = false;
    }

    /// 从源里读一帧（每声道一个采样）；读完补 0
    fn read_frame(src: &mut dyn Source<Item = f32>, dst: &mut [f32]) {
        for k in 0..dst.len() {
            dst[k] = src.next().unwrap_or(0.0);
        }
    }

    /// 产出恰好 `out_frames` 个输出帧到 `out`（交错 f32）。
    ///
    /// 锚点在 cur 上：pos=0 时输出原样的 cur，pos 逼近 1 时趋近 nxt。
    /// 这样 ratio=1（直通）走的是 pos 恒为 0 的路径，输出与输入逐样本相同，
    /// 不会因为"整相位插值取到前一帧"而整体错开一格。
    fn fill(
        &mut self,
        src: &mut dyn Source<Item = f32>,
        out: &mut [f32],
        out_frames: usize,
        out_channels: u16,
    ) {
        let inch = self.in_channels as usize;
        let outch = out_channels.max(1) as usize;
        if self.cur.len() != inch {
            self.cur = vec![0.0; inch];
            self.nxt = vec![0.0; inch];
            self.primed = false;
        }
        if !self.primed {
            Self::read_frame(src, &mut self.cur);
            Self::read_frame(src, &mut self.nxt);
            self.primed = true;
        }

        for f in 0..out_frames {
            let frac = self.pos.clamp(0.0, 1.0) as f32;
            for c in 0..outch {
                let a = self.cur.get(c).copied().unwrap_or(0.0);
                let b = self.nxt.get(c).copied().unwrap_or(a);
                let idx = f * outch + c;
                if idx < out.len() {
                    out[idx] = a + (b - a) * frac;
                }
            }
            self.pos += self.ratio;
            while self.pos >= 1.0 {
                self.cur.copy_from_slice(&self.nxt);
                Self::read_frame(src, &mut self.nxt);
                self.pos -= 1.0;
            }
        }
    }
}

// ---------------- 字节转换 ----------------

/// 把交错的 f32 输出转成设备要的字节格式
struct FmtWriter {
    bytes_per_sample: u16,
    float: bool,
}

impl FmtWriter {
    fn write(&self, samples: &[f32], out: &mut Vec<u8>) {
        if self.float {
            for s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        } else {
            let bits = self.bytes_per_sample * 8;
            for s in samples {
                let v = s.clamp(-1.0, 1.0);
                match bits {
                    16 => {
                        let i = (v * 32767.0) as i16;
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                    24 => {
                        let i = (v * 8_388_607.0) as i32;
                        out.extend_from_slice(&i.to_le_bytes()[..3]);
                    }
                    32 => {
                        let i = (v * 2_147_483_647.0) as i32;
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                    _ => {
                        // 非 16/24/32 位：按 16 位处理，保证不崩
                        let i = (v * 32767.0) as i16;
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                }
            }
        }
    }
}

// ---------------- 渲染线程 ----------------

#[cfg(windows)]
fn render_thread_open(
    device_pref: Option<&str>,
    src_rate: u32,
) -> Result<ExclusiveSink, ExclusiveUnsupported> {
    // 实际初始化全在渲染线程里做（COM 对象不能跨线程），这里只负责起线程
    // 并等它把协商结果回传。

    // COM 对象必须留在创建它的线程上：枚举、协商、渲染全在渲染线程里做
    let shared = std::sync::Arc::new(Shared {
        src: Mutex::new(None),
        volume: AtomicU32::new(0.8f32.to_bits()),
        speed: AtomicU32::new(1.0f32.to_bits()),
        playing: AtomicBool::new(false),
        eof: AtomicBool::new(true),
        stopped: AtomicBool::new(false),
        seek_gen: AtomicU64::new(0),
        shutdown: AtomicBool::new(false),
        thread_error: Mutex::new(None),
    });

    let thread_shared = std::sync::Arc::clone(&shared);
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(u32, String), ExclusiveUnsupported>>();

    let pref = device_pref.map(|s| s.to_string());
    let handle = std::thread::Builder::new()
        .name("yimai-exclusive".into())
        .spawn(move || {
            match run_render_loop(pref, src_rate, &thread_shared) {
                Ok((rate, name)) => {
                    let _ = init_tx.send(Ok((rate, name)));
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| ExclusiveUnsupported::Init(format!("无法创建渲染线程: {e}")))?;

    match init_rx.recv() {
        Ok(Ok((rate, name))) => Ok(ExclusiveSink {
            shared,
            handle: Some(handle),
            device_rate: rate,
            device_name: name,
        }),
        Ok(Err(e)) => {
            // 初始化失败：线程已经自己退出了，这里只需 join 掉
            let _ = handle.join();
            Err(e)
        }
        Err(_) => {
            let _ = handle.join();
            Err(ExclusiveUnsupported::Init("渲染线程无响应".into()))
        }
    }
}

#[cfg(windows)]
fn run_render_loop(
    device_pref: Option<String>,
    src_rate: u32,
    shared: &std::sync::Arc<Shared>,
) -> Result<(u32, String), ExclusiveUnsupported> {
    use wasapi::{
        initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
    };
    use windows::Win32::Foundation::E_INVALIDARG;
    use windows::Win32::Media::Audio::{
        AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE,
        AUDCLNT_E_ENDPOINT_CREATE_FAILED, AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
        AUDCLNT_E_UNSUPPORTED_FORMAT,
    };

    // initialize_mta() 返回 HRESULT 而非 Result
    initialize_mta()
        .ok()
        .map_err(|e| ExclusiveUnsupported::Init(format!("COM 初始化失败: {e:?}")))?;

    let enumerator = DeviceEnumerator::new()
        .map_err(|e| ExclusiveUnsupported::Init(format!("设备枚举失败: {e}")))?;

    // get_device_with_name 挂在设备集合上（不是枚举器本身）
    let device = match device_pref.as_deref() {
        Some(name) => enumerator
            .get_device_collection(&Direction::Render)
            .and_then(|c| c.get_device_with_name(name))
            .map_err(|e| ExclusiveUnsupported::DeviceMissing(format!("{name}（{e}）")))?,
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| ExclusiveUnsupported::Init(format!("取默认输出设备失败: {e}")))?,
    };
    let dev_name = device
        .get_friendlyname()
        .unwrap_or_else(|_| device_pref.clone().unwrap_or_else(|| "默认设备".into()));

    let mut client: AudioClient = device
        .get_iaudioclient()
        .map_err(|e| ExclusiveUnsupported::Init(format!("获取音频客户端失败: {e}")))?;

    // 格式协商：优先"源采样率直通"（32bit float / 双声道），
    // 设备不接受就退回 24bit int，再退回 16bit int。
    let channels = 2usize;
    let candidates: Vec<(usize, usize, SampleType, u32)> = vec![
        (32, 32, SampleType::Float, src_rate.max(8000)),
        (24, 24, SampleType::Int, src_rate.max(8000)),
        (16, 16, SampleType::Int, src_rate.max(8000)),
    ];
    let mut format: Option<WaveFormat> = None;
    for (store, valid, st, rate) in candidates {
        let want = WaveFormat::new(store, valid, &st, rate as usize, channels, None);
        if let Ok(f) = client.is_supported_exclusive_with_quirks(&want) {
            format = Some(f);
            break;
        }
    }
    if format.is_none() {
        // 源采样率不认：退回设备自己的默认格式（get_device_format 挂在 Device 上）
        if let Ok(mix) = device.get_device_format() {
            // 独占不接受自动转换，采样类型固定成 float32
            let f = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                mix.get_samplespersec() as usize,
                channels,
                None,
            );
            if let Ok(ok) = client.is_supported_exclusive_with_quirks(&f) {
                format = Some(ok);
            }
        }
    }
    let Some(format) = format else {
        return Err(ExclusiveUnsupported::NoFormat(format!(
            "设备「{dev_name}」不接受 32/24/16bit 的 {}Hz 独占格式",
            src_rate.max(8000)
        )));
    };

    let blockalign = format.get_blockalign() as usize;
    let dev_rate = format.get_samplespersec();
    let dev_channels = format.get_nchannels().max(1) as usize;
    let sample_type = format
        .get_subformat()
        .map_err(|e| ExclusiveUnsupported::Init(format!("读取设备采样格式失败: {e}")))?;

    let (def_period, _min_period) = client
        .get_device_period()
        .map_err(|e| ExclusiveUnsupported::Init(format!("读取设备周期失败: {e}")))?;
    // 128 字节对齐是 Intel HDA 等设备的硬性要求，不对齐会 Initialize 失败
    let period = client
        .calculate_aligned_period_near(def_period, Some(128), &format)
        .map_err(|e| ExclusiveUnsupported::Init(format!("计算缓冲周期失败: {e}")))?;

    let mut init_mode = StreamMode::PollingExclusive {
        period_hns: period,
        buffer_duration_hns: 16 * period,
    };
    let mut aligned_retry = false;
    loop {
        match client.initialize_client(&format, &Direction::Render, &init_mode) {
            Ok(()) => break,
            Err(wasapi::WasapiError::Windows(werr)) => {
                // wasapi 依赖 windows 0.62、本项目 windows 0.61，两边的 HRESULT
                // 是不同的 newtype，不能直接比大小。HRESULT 只是 i32 包装，
                // 按 .0 比较即可跨版本通用。
                let code = werr.code().0;
                if code == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED.0 && !aligned_retry {
                    // 官方推荐的纠偏：按 GetBufferSize 重算对齐周期后重试一次
                    aligned_retry = true;
                    if let Ok(frames) = client.get_buffer_size() {
                        let aligned =
                            wasapi::calculate_period_100ns(frames as i64, dev_rate as i64);
                        init_mode = StreamMode::PollingExclusive {
                            period_hns: aligned,
                            buffer_duration_hns: 16 * aligned,
                        };
                        client = device
                            .get_iaudioclient()
                            .map_err(|e| {
                                ExclusiveUnsupported::Init(format!("重建音频客户端失败: {e}"))
                            })?;
                        continue;
                    }
                    return Err(ExclusiveUnsupported::Init(werr.message().to_string()));
                } else if code == AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED.0 {
                    return Err(ExclusiveUnsupported::NotAllowed(
                        "驱动拒绝独占（蓝牙耳机、网络音箱、多数 HDMI/显示器输出均不支持）".into(),
                    ));
                } else if code == AUDCLNT_E_DEVICE_IN_USE.0 {
                    return Err(ExclusiveUnsupported::NotAllowed(
                        "设备已被其它独占程序占用".into(),
                    ));
                } else if code == AUDCLNT_E_UNSUPPORTED_FORMAT.0 {
                    return Err(ExclusiveUnsupported::NoFormat(
                        "设备不支持该独占输出格式".into(),
                    ));
                } else if code == AUDCLNT_E_ENDPOINT_CREATE_FAILED.0 {
                    return Err(ExclusiveUnsupported::Init(format!(
                        "音频端点创建失败：{}",
                        werr.message()
                    )));
                } else if code == E_INVALIDARG.0 {
                    return Err(ExclusiveUnsupported::NoFormat(
                        "设备拒绝了该参数组合（采样率/位深/声道）".into(),
                    ));
                } else {
                    return Err(ExclusiveUnsupported::Init(format!(
                        "HRESULT {:#010x}: {}",
                        code,
                        werr.message()
                    )));
                }
            }
            Err(e) => return Err(ExclusiveUnsupported::Init(format!("{e}"))),
        }
    }

    let render_client = client
        .get_audiorenderclient()
        .map_err(|e| ExclusiveUnsupported::Init(format!("获取渲染端点失败: {e}")))?;
    let buffer_frames = client
        .get_buffer_size()
        .map_err(|e| ExclusiveUnsupported::Init(format!("读取缓冲大小失败: {e}")))?
        .max(1024);

    client
        .start_stream()
        .map_err(|e| ExclusiveUnsupported::Init(format!("启动独占流失败: {e}")))?;

    let writer = FmtWriter {
        bytes_per_sample: format.get_bitspersample() / 8,
        float: matches!(sample_type, SampleType::Float),
    };

    // 睡眠半个缓冲周期，既不空转也不会错过补数据的时机
    let sleep = Duration::from_nanos(
        500_000_000u64 * buffer_frames as u64 / dev_rate.max(1) as u64,
    );

    let mut resampler = Resampler::new(2);
    let mut out_f32: Vec<f32> = Vec::new();
    let mut out_bytes: Vec<u8> = Vec::new();
    let mut last_seen_gen = u64::MAX;
    let mut silence = vec![0f32; buffer_frames as usize * dev_channels];

    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(sleep);

        let space = match client.get_available_space_in_frames() {
            Ok(s) => s.max(0),
            Err(_) => break,
        };
        if space == 0 {
            continue;
        }
        let frames = space.min(buffer_frames) as usize;

        let playing = shared.playing.load(Ordering::SeqCst);
        let volume = f32::from_bits(shared.volume.load(Ordering::SeqCst));
        let speed = f32::from_bits(shared.speed.load(Ordering::SeqCst));

        let gen = shared.seek_gen.load(Ordering::SeqCst);
        if gen != last_seen_gen {
            resampler.reset();
            last_seen_gen = gen;
        }

        let mut src_guard = shared.src.lock();
        let Some(src) = src_guard.as_mut() else {
            shared.eof.store(true, Ordering::SeqCst);
            continue;
        };
        let out_channels = src.channels();

        if !playing {
            // 暂停：继续喂静音但不消费源，保证进度不前进、流不中断
            out_bytes.clear();
            silence.truncate(frames * dev_channels);
            writer.write(&silence, &mut out_bytes);
        } else {
            // ratio = 每输出一个设备采样要消耗多少源采样
            // = (源采样率 × 倍速) / 设备采样率
            let ratio = (src_rate as f64 * speed as f64) / dev_rate as f64;
            resampler.in_channels = out_channels;
            resampler.set_ratio(ratio);
            out_f32.clear();
            out_f32.resize(frames * dev_channels, 0.0);
            resampler.fill(src.as_mut(), &mut out_f32, frames, dev_channels as u16);
            for s in out_f32.iter_mut() {
                *s *= volume;
            }
            out_bytes.clear();
            writer.write(&out_f32, &mut out_bytes);
        }
        drop(src_guard);

        if out_bytes.len() == frames * blockalign
            && render_client.write_to_device(frames, &out_bytes, None).is_err()
        {
            // 写失败通常意味着设备被拔掉或被系统抢回（有人开了共享流）
            *shared.thread_error.lock() =
                Some("独占流被中断（设备被拔出或已被其它程序占用）".into());
            shared.playing.store(false, Ordering::SeqCst);
            shared.eof.store(true, Ordering::SeqCst);
            break;
        }
    }

    let _ = client.stop_stream();
    Ok((dev_rate, dev_name))
}

#[cfg(not(windows))]
fn render_thread_open(
    _device_pref: Option<&str>,
    _src_rate: u32,
) -> Result<ExclusiveSink, ExclusiveUnsupported> {
    Err(ExclusiveUnsupported::Init(
        "独占模式仅支持 Windows".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的斜坡音源：依次产出 0.0, 1/n, 2/n …
    struct Ramp {
        data: Vec<f32>,
        pos: usize,
        ch: u16,
    }
    impl Ramp {
        fn new(n: usize, ch: u16) -> Self {
            Ramp {
                data: (0..n).map(|i| (i as f32) / n as f32).collect(),
                pos: 0,
                ch,
            }
        }
        fn remaining(&self) -> usize {
            self.data.len() - self.pos
        }
    }
    impl Iterator for Ramp {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            let v = self.data.get(self.pos).copied();
            if v.is_some() {
                self.pos += 1;
            }
            v
        }
    }
    impl Source for Ramp {
        fn current_frame_len(&self) -> Option<usize> {
            None
        }
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn channels(&self) -> u16 {
            self.ch
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    /// ratio=1（直通）时输出必须与输入逐样本相同。
    /// 这是"采样率直通、不重采样"的前提：一旦错开一帧就会引入相位偏移。
    #[test]
    fn resampler_passthrough_at_ratio_one() {
        let mut r = Resampler::new(1);
        r.set_ratio(1.0);
        let mut src = Ramp::new(64, 1);
        let mut out = vec![0f32; 16];
        r.fill(&mut src, &mut out, 16, 1);
        for f in 0..16 {
            let want = f as f32 / 64.0;
            assert!(
                (out[f] - want).abs() < 1e-6,
                "第 {f} 帧应为 {want}，实际 {}",
                out[f]
            );
        }
    }

    /// ratio=2（慢放）：每输出 1 帧吃 2 个输入，消费必须明显更快
    #[test]
    fn resampler_higher_ratio_consumes_more() {
        let consumed = |ratio: f64| -> usize {
            let mut r = Resampler::new(1);
            r.set_ratio(ratio);
            let mut src = Ramp::new(256, 1);
            let mut out = vec![0f32; 16];
            r.fill(&mut src, &mut out, 16, 1);
            256 - src.remaining()
        };
        let fast = consumed(1.0);
        let slow = consumed(2.0);
        assert!(slow >= 32, "ratio=2 至少消费 32 个输入，实际 {slow}");
        assert!(slow > fast, "慢放应比直通消费更多：{slow} vs {fast}");
    }

    /// ratio=0.5（快放）：消费应明显少于直通
    #[test]
    fn resampler_lower_ratio_consumes_less() {
        let consumed = |ratio: f64| -> usize {
            let mut r = Resampler::new(1);
            r.set_ratio(ratio);
            let mut src = Ramp::new(256, 1);
            let mut out = vec![0f32; 16];
            r.fill(&mut src, &mut out, 16, 1);
            256 - src.remaining()
        };
        let fast = consumed(1.0);
        let quicker = consumed(0.5);
        assert!(
            quicker < fast,
            "快放应消费更少：{quicker} vs {fast}"
        );
    }

    /// 双声道：左右声道不能串到一起
    #[test]
    fn resampler_stereo_channels_stay_separate() {
        let mut r = Resampler::new(2);
        r.set_ratio(1.0);
        // 交替 -1 / +1，串道会立刻表现为两声道相同
        let src_data: Vec<f32> = (0..8).map(|i| if i % 2 == 0 { -1.0 } else { 1.0 }).collect();
        let mut src = Ramp {
            data: src_data,
            pos: 0,
            ch: 2,
        };
        let mut out = vec![0f32; 8];
        r.fill(&mut src, &mut out, 4, 2);
        for f in 0..4 {
            assert!(
                (out[f * 2] - out[f * 2 + 1]).abs() > 0.5,
                "第 {f} 帧左右声道串了：{} vs {}",
                out[f * 2],
                out[f * 2 + 1]
            );
        }
        // 且直通时左声道应一路保持 -1
        for f in 0..4 {
            assert!((out[f * 2] + 1.0).abs() < 1e-6, "左声道第 {f} 帧被改了");
        }
    }

    /// 源耗尽后补 0，不应崩溃也不应拖长最后一个样本
    #[test]
    fn resampler_pads_with_zeros_when_source_ends() {
        let mut r = Resampler::new(1);
        r.set_ratio(1.0);
        let mut src = Ramp::new(4, 1);
        let mut out = vec![9.0f32; 16];
        r.fill(&mut src, &mut out, 16, 1);
        assert_eq!(src.remaining(), 0);
        // 前 4 帧是原始数据（0, .25, .5, .75），之后必须归零
        let want = [0.0f32, 0.25, 0.5, 0.75];
        for (i, w) in want.iter().enumerate() {
            assert!((out[i] - w).abs() < 1e-6, "第 {i} 帧应为 {w}，实际 {}", out[i]);
        }
        assert!(
            out[4..].iter().all(|v| *v == 0.0),
            "源耗尽后应补 0，实际 {:?}",
            &out[4..8]
        );
    }

    #[test]
    fn f32_to_pcm16_conversion() {
        let w = FmtWriter {
            bytes_per_sample: 2,
            float: false,
        };
        let mut out = Vec::new();
        w.write(&[0.0, 1.0, -1.0], &mut out);
        assert_eq!(out.len(), 6);
        let a = i16::from_le_bytes([out[0], out[1]]);
        let b = i16::from_le_bytes([out[2], out[3]]);
        let c = i16::from_le_bytes([out[4], out[5]]);
        assert_eq!(a, 0);
        assert_eq!(b, 32767);
        assert_eq!(c, -32767);
    }

    #[test]
    fn f32_float32_passthrough() {
        let w = FmtWriter {
            bytes_per_sample: 4,
            float: true,
        };
        let mut out = Vec::new();
        w.write(&[0.5, -0.25], &mut out);
        assert_eq!(f32::from_le_bytes([out[0], out[1], out[2], out[3]]), 0.5);
        assert_eq!(
            f32::from_le_bytes([out[4], out[5], out[6], out[7]]),
            -0.25
        );
    }
}
