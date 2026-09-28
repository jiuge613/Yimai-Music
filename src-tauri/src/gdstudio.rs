//! GD音乐台开放 API 客户端——**内置聚合音源**（搜索 / 取链 / 封面 / 歌词兜底）。
//!
//! 用途：
//! - 排行榜在未接入任何网络音源时的默认搜索与播放来源（search / url）；
//! - 播放在线曲目时按曲目 id 拉取专辑封面（pic）；
//! - 当本地 .lrc / 内嵌标签 / 平台接口 / 网易云备用源全部拿不到歌词时，
//!   用「歌名 + 歌手」在这里检索并取回 LRC（search + lyric）。
//!
//! ## 接口契约（2026-09 实测确认）
//!
//! - 搜索：`GET {base}?types=search&source=<src>&name=<关键词>&count=<n>&pages=<p>`
//!   → `[{ "id", "name", "artist": [..], "album", "pic_id", "url_id", "lyric_id", "source" }]`
//! - 播放直链：`GET {base}?types=url&source=<src>&id=<url_id|id>&br=<kbps>`
//!   → `{ "url": "https://…", "br": 1497, "size": 21041357 }`
//!   （`br` 必须是整数；该端点对非浏览器 UA 返回 503，故请求需带浏览器 UA）
//! - 封面：`GET {base}?types=pic&source=<src>&id=<pic_id>&size=<边长>`
//!   → `{ "url": "https://…jpg?param=500y500" }`
//! - 歌词：`GET {base}?types=lyric&source=<src>&id=<曲目id>`
//!   → `{ "lyric": "[00:00.000]…" }`（标准 LRC）
//!
//! ## 授权（重要）
//!
//! 接口方声明 **CC BY-NC 4.0**、仅供学习参考、禁止商用与传播，并要求注明出处
//! "GD音乐台(music.gdstudio.xyz)"。该源在本应用中作为**内置默认源**启用，
//! 应用内不提供关闭开关（设置页不展示），出处标注保留在排行榜页与本注释中。
//!
//! 限流：官方标注 5 分钟不超过 50 次请求，失败一律静默降级，绝不阻断播放。

use std::time::Duration;

use serde::Deserialize;

/// 官方默认端点（可被用户设置覆盖）
pub const DEFAULT_BASE: &str = "https://music-api.gdstudio.xyz/api.php";
/// 出处标注，设置页与本模块注释均需保留
pub const ATTRIBUTION: &str = "GD音乐台 (music.gdstudio.xyz)";

/// 检索曲目
#[derive(Debug, Clone, Deserialize)]
pub struct GdSong {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub artist: Vec<String>,
    #[serde(default)]
    pub album: String,
    /// 取封面用（types=pic 的 id），缺失时退回 id
    #[serde(default)]
    pub pic_id: String,
    /// 取播放直链用（types=url 的 id），缺失时退回 id
    #[serde(default)]
    pub url_id: String,
    /// 官方字段里取歌词用的是 lyric_id，缺失时退回 id
    #[serde(default)]
    pub lyric_id: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Deserialize)]
struct LyricResp {
    #[serde(default)]
    lyric: String,
}

#[derive(Debug, Deserialize)]
struct UrlResp {
    #[serde(default)]
    url: String,
    #[serde(default)]
    br: u64,
}

#[derive(Debug, Deserialize)]
struct PicResp {
    #[serde(default)]
    url: String,
}

/// 校验并规整用户配置的服务端地址。
///
/// 端点可配置 ⇒ 必须做 SSRF 防护：仅允许 http/https，且拒绝环回 / 私有 /
/// 链路本地 / 保留地址。否则一个恶意或误配的地址就能让本机去探测内网服务
/// （`http://127.0.0.1:8080`、`http://169.254.169.254/…` 等）。
pub fn normalize_base(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("接口地址为空".into());
    }
    // 必须带 scheme；不补全，避免 "evil.com" 被当成相对路径
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("接口地址必须以 http:// 或 https:// 开头".into());
    }
    let rest = match lower.find("://") {
        Some(i) => &s[i + 3..],
        None => return Err("接口地址格式不正确".into()),
    };
    // 取 host[:port]，去掉 userinfo、路径、查询
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 去掉 userinfo（防止 "evil.com@127.0.0.1" 这类绕过）
    let hostport = authority.rsplit('@').next().unwrap_or("");
    if hostport.is_empty() {
        return Err("接口地址缺少主机名".into());
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        _ => (hostport, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host.is_empty() {
        return Err("接口地址缺少主机名".into());
    }
    if let Some(p) = port {
        let n: u32 = p.parse().map_err(|_| "接口地址端口无效")?;
        if n == 0 || n > 65535 {
            return Err("接口地址端口无效".into());
        }
    }
    if is_blocked_host(&host) {
        return Err(format!("出于安全考虑，不允许使用内网/保留地址：{host}"));
    }
    Ok(s.trim_end_matches('/').to_string())
}

/// 判断主机是否属于环回 / 私有 / 链路本地 / 保留 / 通配地址。
fn is_blocked_host(host: &str) -> bool {
    // IPv6 字面量（[::1]、[::ffff:127.0.0.1]、[fe80::1]…）：单独判定，
    // 否则会掉进下面的"当主机名处理"分支而漏掉环回与内网段。
    if host.contains(':') {
        return is_blocked_ipv6(host);
    }
    // 非 IP 的主机名（如官方域名）放行：DNS 解析后的地址无法在此静态判断，
    // 真正的兜底是"只允许用户显式配置"，而不是替用户决定连哪。
    let Some(octets) = parse_ipv4(host) else {
        return host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".local")
            || host.ends_with(".internal");
    };
    let [a, b, ..] = octets;
    match a {
        0 => true,                             // 0.0.0.0/8 本机
        10 => true,                            // 私有
        127 => true,                           // 环回
        169 if b == 254 => true,               // 链路本地（含云元数据 169.254.169.254）
        172 if (16..=31).contains(&b) => true, // 私有
        192 if b == 168 => true,               // 私有
        100 if (64..=127).contains(&b) => true, // CGNAT 100.64/10
        192 if b == 0 => true,                 // IETF 协议保留 192.0.0/24
        198 if b == 18 || b == 19 => true,     // 基准测试 198.18/15
        _ if a >= 224 => true,                 // 组播 224+/保留 240+
        _ => false,
    }
}

/// IPv6 危险段判定（不求完整解析，只覆盖真实可用的绕过面）。
fn is_blocked_ipv6(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    // IPv4-mapped / 兼容形式 ::ffff:127.0.0.1 —— 内嵌 IPv4，按 IPv4 规则判
    if let Some(v4) = h.rsplit("::ffff:").next().filter(|s| *s != h) {
        if let Some(o) = parse_ipv4(v4) {
            return is_blocked_ipv4(o);
        }
    }
    if h == "::" || h == "::1" {
        return true; // 未指定 / 环回
    }
    // fe80::/10 链路本地、fc00::/7 唯一本地、fec0::/10 站点本地
    let head = h.split(':').next().unwrap_or("");
    if head.len() >= 2 {
        if let Ok(p) = u32::from_str_radix(&head[..2], 16) {
            if (0xfe8..=0xfeb).contains(&p) {
                return true;
            }
            if (0xfc..=0xfd).contains(&p) || (0xfec..=0xfef).contains(&p) {
                return true;
            }
        }
    }
    // 其余 IPv6 一律不放行：兜底歌词源用不到裸 IPv6 字面量，
    // 放行的风险（DNS64/映射绕行）远大于收益。
    true
}

fn is_blocked_ipv4(o: [u32; 4]) -> bool {
    let [a, b, ..] = o;
    match a {
        0 | 10 | 127 => true,
        169 if b == 254 => true,
        172 if (16..=31).contains(&b) => true,
        192 if b == 168 || b == 0 => true,
        100 if (64..=127).contains(&b) => true,
        198 if b == 18 || b == 19 => true,
        _ if a >= 224 => true,
        _ => false,
    }
}

fn parse_ipv4(host: &str) -> Option<[u32; 4]> {
    // 只认纯 IPv4 字面量；含其它字符的一律当主机名处理
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u32; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        out[i] = p.parse().ok()?;
        if out[i] > 255 {
            return None;
        }
    }
    Some(out)
}

fn agent() -> ureq::Agent {
    // url 端点对非浏览器 UA 返回 503（Cloudflare 防护，实测），须带浏览器 UA
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(8))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 YimaiMusic")
        .build()
}

/// 校验取链/封面接口返回的直链，满足服务端请求的安全约束：
/// 仅允许 http/https，且 host 不得是环回 / 私有 / 链路本地 / 保留地址。
/// 这些直链随后会被引擎下载（服务端请求），必须与 GD 端点同等的 SSRF 防护。
pub fn validate_remote_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("音源返回的链接必须以 http:// 或 https:// 开头".into());
    }
    let rest = &u[lower.find("://").unwrap() + 3..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let (host, _port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        _ => (hostport, None),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host.is_empty() {
        return Err("音源返回的链接缺少主机名".into());
    }
    if is_blocked_host(&host) {
        return Err(format!("音源返回了不允许的内网/保留地址：{host}"));
    }
    Ok(())
}

/// 按歌名/歌手检索。返回空 vec 表示无结果或失败（兜底语义，不抛错）。
pub fn search(base: &str, source: &str, keyword: &str, count: u32) -> Vec<GdSong> {
    let base = match normalize_base(base) {
        Ok(b) => b,
        Err(_) => return vec![],
    };
    if keyword.trim().is_empty() {
        return vec![];
    }
    let url = format!(
        "{base}?types=search&source={}&name={}&count={}",
        urlencode(source),
        urlencode(keyword),
        count.clamp(1, 20)
    );
    match agent().get(&url).call() {
        Ok(resp) => match resp.into_json::<Vec<GdSong>>() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gd] 搜索响应解析失败: {e}");
                vec![]
            }
        },
        Err(e) => {
            eprintln!("[gd] 搜索请求失败: {e}");
            vec![]
        }
    }
}

/// 按曲目 id 取 LRC 原文。失败返回空串（兜底语义，不抛错）。
pub fn lyric(base: &str, source: &str, id: &str) -> String {
    let base = match normalize_base(base) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[gd] {e}");
            return String::new();
        }
    };
    if id.trim().is_empty() {
        return String::new();
    }
    let url = format!(
        "{base}?types=lyric&source={}&id={}",
        urlencode(source),
        urlencode(id)
    );
    match agent().get(&url).call() {
        Ok(resp) => match resp.into_json::<LyricResp>() {
            Ok(v) => v.lyric,
            Err(e) => {
                eprintln!("[gd] 歌词响应解析失败: {e}");
                String::new()
            }
        },
        Err(e) => {
            eprintln!("[gd] 歌词请求失败: {e}");
            String::new()
        }
    }
}

/// 按曲目 id 取播放直链，返回 (直链, 实际码率 kbps)。
/// `br` 为期望码率上限（999 = 不限），接口返回不超过该值的最佳音质。
/// 失败返回 Err（播放主链路，需要把失败原因呈现给用户）。
pub fn song_url(base: &str, source: &str, id: &str, br: u32) -> Result<(String, u64), String> {
    let base = normalize_base(base)?;
    if id.trim().is_empty() {
        return Err("曲目 ID 为空，无法取链".into());
    }
    let url = format!(
        "{base}?types=url&source={}&id={}&br={}",
        urlencode(source),
        urlencode(id),
        br.clamp(128, 999)
    );
    let resp = agent()
        .get(&url)
        .call()
        .map_err(|e| format!("GD音乐台取链失败: {e}"))?;
    let v: UrlResp = resp.into_json().map_err(|e| format!("取链响应解析失败: {e}"))?;
    if v.url.trim().is_empty() {
        return Err("该歌曲暂无可播放链接".into());
    }
    validate_remote_url(&v.url)?;
    Ok((v.url, v.br))
}

/// 按封面 id 取专辑图直链。失败返回空串（封面缺失只是观感问题，静默降级）。
pub fn pic_url(base: &str, source: &str, id: &str) -> String {
    let base = match normalize_base(base) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    if id.trim().is_empty() {
        return String::new();
    }
    let url = format!(
        "{base}?types=pic&source={}&id={}&size=500",
        urlencode(source),
        urlencode(id)
    );
    let ok = agent()
        .get(&url)
        .call()
        .ok()
        .and_then(|r| r.into_json::<PicResp>().ok())
        .map(|p| p.url)
        .unwrap_or_default();
    if ok.trim().is_empty() {
        return String::new();
    }
    // 直链最终由前端 WebView 加载，但来源标记会入库/复制到别处，
    // 这里同样过一遍安全校验，杜绝内网地址借封面混进来
    if validate_remote_url(&ok).is_err() {
        return String::new();
    }
    ok
}

/// 归一化：小写 + 只留字母数字/汉字，用于歌名歌手匹配
pub fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// 在候选里挑最匹配的一首，返回 (id, source)。
///
/// 打分与网易云兜底同思路：歌名相等 3 分 / 互含 1 分；歌手相等 4 分 / 互含 2 分。
/// 额外要求歌名必须命中或歌手必须命中，避免把同名不同唱的歌词配错。
pub fn pick<'a>(songs: &'a [GdSong], title: &str, artist: &str) -> Option<&'a GdSong> {
    let target = norm(title);
    let target_a = norm(artist);
    if target.is_empty() {
        return None;
    }
    let mut best: Option<(&GdSong, i32)> = None;
    for s in songs {
        let name = norm(&s.name);
        if name.is_empty() {
            continue;
        }
        let mut score = 0i32;
        if name == target {
            score += 3;
        } else if name.contains(&target) || target.contains(&name) {
            score += 1;
        }
        if !target_a.is_empty() {
            let arts: Vec<String> = s.artist.iter().map(|a| norm(a)).collect();
            if arts.iter().any(|a| a == &target_a) {
                score += 4;
            } else if arts
                .iter()
                .any(|a| !a.is_empty() && (a.contains(&target_a) || target_a.contains(a.as_str())))
            {
                score += 2;
            }
        }
        if name == target || (score >= 1 && !target_a.is_empty()) {
            if best.map(|(_, b)| score > b).unwrap_or(true) {
                best = Some((s, score));
            }
        }
    }
    best.map(|(s, _)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_must_be_http() {
        assert!(normalize_base("ftp://a.com/x").is_err());
        assert!(normalize_base("file:///c:/x").is_err());
        assert!(normalize_base("javascript:alert(1)").is_err());
        assert!(normalize_base("a.com/api.php").is_err(), "无 scheme 应拒绝");
        assert!(normalize_base("").is_err());
        assert!(normalize_base("https://music-api.gdstudio.xyz/api.php").is_ok());
    }

    #[test]
    fn blocks_loopback_private_and_reserved() {
        for h in [
            "http://127.0.0.1/api.php",
            "http://localhost:8080/api.php",
            "http://10.0.0.5/api.php",
            "http://172.16.3.4/api.php",
            "http://192.168.1.1/api.php",
            "http://169.254.169.254/latest/meta-data",
            "http://0.0.0.0/api.php",
            "http://100.64.0.1/api.php",
            "http://198.18.0.1/api.php",
            "http://255.255.255.255/api.php",
            "http://foo.localhost/api.php",
            // IPv6：环回 / 未指定 / 链路本地 / 唯一本地 / IPv4 映射回环
            "http://[::1]/api.php",
            "http://[::]/api.php",
            "http://[fe80::1]/api.php",
            "http://[fc00::1]/api.php",
            "http://[::ffff:127.0.0.1]/api.php",
            "http://[::ffff:192.168.0.1]/api.php",
        ] {
            assert!(normalize_base(h).is_err(), "{h} 应被拒绝");
        }
    }

    #[test]
    fn blocks_userinfo_smuggling() {
        // "evil.com@127.0.0.1" 真实主机是 127.0.0.1，不能被当成 evil.com 放行
        assert!(normalize_base("http://evil.com@127.0.0.1/api.php").is_err());
    }

    #[test]
    fn allows_normal_hosts() {
        assert!(normalize_base("http://music-api.gdstudio.xyz/api.php").is_ok());
        assert!(normalize_base("https://my-mirror.example.cn/api.php?x=1").is_ok());
        assert!(normalize_base("https://8.8.8.8/api.php").is_ok());
    }

    #[test]
    fn pick_prefers_exact_title_and_artist() {
        let songs = vec![
            GdSong {
                id: "1".into(),
                name: "晴天".into(),
                artist: vec!["周杰倫".into()],
                album: String::new(),
                pic_id: String::new(),
                url_id: String::new(),
                lyric_id: String::new(),
                source: "netease".into(),
            },
            GdSong {
                id: "2".into(),
                name: "晴天 (深情版)".into(),
                artist: vec!["别人".into()],
                album: String::new(),
                pic_id: String::new(),
                url_id: String::new(),
                lyric_id: String::new(),
                source: "netease".into(),
            },
        ];
        let hit = pick(&songs, "晴天", "周杰伦").expect("应命中");
        assert_eq!(hit.id, "1");
    }

    #[test]
    fn pick_rejects_wrong_song() {
        let songs = vec![GdSong {
            id: "9".into(),
            name: "完全不同的歌".into(),
            artist: vec!["某歌手".into()],
            album: String::new(),
            pic_id: String::new(),
            url_id: String::new(),
            lyric_id: String::new(),
            source: "netease".into(),
        }];
        assert!(pick(&songs, "晴天", "周杰伦").is_none());
    }

    #[test]
    fn pick_rejects_empty_title() {
        assert!(pick(&[], "", "周杰伦").is_none());
    }

    /// 取链/封面返回的直链最终由引擎下载（服务端请求），
    /// 必须与 GD 端点同等的 SSRF 防护
    #[test]
    fn remote_url_validation() {
        assert!(validate_remote_url("https://p2.music.126.net/x.jpg").is_ok());
        assert!(validate_remote_url("http://m701.music.126.net/a.flac").is_ok());
        for bad in [
            "ftp://music.126.net/a.mp3",
            "javascript:alert(1)",
            "http://127.0.0.1/x.mp3",
            "http://localhost/x.mp3",
            "http://192.168.1.2/a.flac",
            "http://169.254.169.254/latest",
            "https://[::1]/a.flac",
            "http://10.0.0.3/x",
            "",
        ] {
            assert!(validate_remote_url(bad).is_err(), "{bad} 应被拒绝");
        }
    }

    /// 真实接口联调：搜索 → 取链 → 封面全链路。
    /// 默认 --ignored 跳过；开启：GD_LIVE=1 cargo test ... -- --ignored --nocapture
    #[test]
    #[ignore = "需要外网；用 GD_LIVE=1 显式开启"]
    fn live_search_url_pic() {
        if std::env::var("GD_LIVE").is_err() {
            eprintln!("跳过：未设置 GD_LIVE=1");
            return;
        }
        let songs = search(super::DEFAULT_BASE, "netease", "晴天 周杰伦", 5);
        assert!(!songs.is_empty(), "搜索应有结果");
        let hit = pick(&songs, "晴天", "周杰伦").expect("应命中");
        let url_id = if hit.url_id.trim().is_empty() { &hit.id } else { &hit.url_id };
        let (u, br) = song_url(super::DEFAULT_BASE, &hit.source, url_id, 999).expect("取链应成功");
        eprintln!("直链={u} 码率={br}");
        assert!(u.starts_with("http"), "直链应为 http/https");
        let pic_id = if hit.pic_id.trim().is_empty() { &hit.id } else { &hit.pic_id };
        let pic = pic_url(super::DEFAULT_BASE, &hit.source, pic_id);
        eprintln!("封面={pic}");
        assert!(pic.starts_with("http"), "封面应为 http/https 直链");
    }

    /// 真实接口联调：跑一遍「搜索 → 选歌 → 取歌词」全链路。
    /// 默认 --ignored 跳过，避免每次 cargo test 都打外网；
    /// 开启：GD_LIVE=1 cargo test --bin yimai gdstudio::tests::live -- --ignored --nocapture
    #[test]
    #[ignore = "需要外网；用 GD_LIVE=1 显式开启"]
    fn live_search_then_lyric() {
        if std::env::var("GD_LIVE").is_err() {
            eprintln!("跳过：未设置 GD_LIVE=1");
            return;
        }
        let (title, artist) = ("晴天", "周杰伦");
        let kw = format!("{title} {artist}");
        let songs = search(super::DEFAULT_BASE, "netease", &kw, 10);
        assert!(!songs.is_empty(), "搜索应返回结果：{kw}");
        eprintln!("搜索「{kw}」得到 {} 条：", songs.len());
        for s in songs.iter().take(5) {
            eprintln!("  id={} name={:?} artist={:?} source={}", s.id, s.name, s.artist, s.source);
        }
        let hit = pick(&songs, title, artist).expect("应能选出匹配曲目");
        eprintln!("选中 id={} name={:?} artist={:?}", hit.id, hit.name, hit.artist);
        assert_eq!(hit.name.trim(), title, "应精确匹配到《晴天》");

        let id = if hit.lyric_id.trim().is_empty() { &hit.id } else { &hit.lyric_id };
        let src = if hit.source.trim().is_empty() { "netease" } else { &hit.source };
        let lrc = lyric(super::DEFAULT_BASE, src, id);
        assert!(!lrc.trim().is_empty(), "应取到歌词原文");
        eprintln!("歌词前 120 字符：\n{}", &lrc[..lrc.len().min(120)]);
        assert!(lrc.contains('['), "应为带时间轴的 LRC");
    }
}
