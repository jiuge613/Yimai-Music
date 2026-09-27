//! 音源脚本（本地导入 / .js 订阅）与 HTTP 接口音源
//!
//! 设计取舍：**不内嵌 JS 运行时**。业界（lx-music-desktop）的音源脚本跑在
//! 沙箱里、通过 `globalThis.lx` 与宿主交互；其中占绝大多数的是「HTTP 接口
//! 型」脚本——脚本本身只做"拼 query 参数 → GET 接口 → 从 JSON 里取直链"，
//! 不含加密运算。对这类脚本，Yimai Music 在导入时**静态解析出契约**
//! （API_BASE、平台/音质声明），播放取链由本模块用 ureq 原生完成，效果与
//! 执行脚本一致而无需沙箱。
//!
//! 导入校验原则：**宽进**。只要是结构合法的音源脚本就允许导入——不强制
//! 引用 `globalThis.lx`（那是 LX 宿主专属），也不强制 `musicUrl` 动作名，
//! 只要文件具备任意一项音源脚本特征（取链方法 / 接口路径 / 平台声明 /
//! 接口地址常量）并能解析出取链接口基址即可。仅对"根本不是脚本""结构不
//! 像音源脚本""解析不出接口地址"三类情况报错。
//!
//! 支持的接口协议（与 lx-online-api 系部署一致）：
//! - `GET {base}/url.php?source=&id=&quality=&[extra=]` → `{code:0,data:{url}}`
//! - `GET {base}/lyric.php?source=&id=` → `{code:0,data:{lyric,tlyric,…}}`
//! - `GET {base}/pic.php?source=&id=`   → `{code:0,data:{url}}`
//! - `GET {base}/platforms.php`（可选）→ `{code:0,data:{platforms:[…]}}`
//!
//! 含加密逻辑、依赖 crypto/zlib 的脚本无法静态执行，导入时会给出明确报错。

use std::time::Duration;

use regex::Regex;

use crate::models::{LxPlatform, LxSearchSong};

const TIMEOUT_CONNECT: Duration = Duration::from_secs(10);
const TIMEOUT_READ: Duration = Duration::from_secs(15);

const UA: &str = "YimaiMusic/1.0 (LX-compatible source client)";

/// 已知平台代码 → 中文名（脚本/platforms.php 未提供名字时兜底）
const PLATFORM_NAMES: &[(&str, &str)] = &[
    ("wy", "网易云"),
    ("tx", "QQ"),
    ("kw", "酷我"),
    ("kg", "酷狗"),
    ("mg", "咪咕"),
    ("joox", "JOOX"),
    ("xm", "小咪"),
    ("bd", "百度"),
];

fn platform_name(code: &str) -> String {
    PLATFORM_NAMES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| code.to_string())
}

// ---------- 脚本解析 ----------

/// 解析结果：从脚本文本中静态提取的契约
#[derive(Debug)]
pub struct ParsedScript {
    pub name: String,
    pub base_url: String,
    pub platforms: Vec<LxPlatform>,
    /// 取链协议模式："" = 标准 LX 协议；"v1" = 自定义 NestJS 端点。
    /// 由前端执行混淆脚本后回填（extract_base 静态无解时）。
    pub api_mode: String,
}

/// 解析音源脚本文本，提取 API_BASE 与平台/音质契约。
///
/// 校验链（每步失败都返回可操作的中文错误）：
/// 1. 内容非空，且不是网页 / JSON / XML 这类非脚本文档；
/// 2. 具备音源脚本的结构特征之一（取链方法、接口路径、平台声明、接口地址常量）；
///    —— 刻意不强制 `globalThis.lx` 与 `musicUrl`，普通音源脚本同样可导入；
/// 3. 能定位取链接口基址（API_BASE 常量或脚本内首个 https 字面量）。
pub fn parse_script(text: &str) -> Result<ParsedScript, String> {
    parse_script_impl(text, None, None)
}

/// 带"前端回填提示"的解析入口：混淆脚本（基址运行时解码）经前端执行取出基址后，
/// 把 `hint_base` / `hint_mode` 传回，跳过静态 `extract_base`，直接用前端结果。
/// 无提示时行为与 `parse_script` 完全一致。
pub fn parse_script_impl(
    text: &str,
    hint_base: Option<&str>,
    hint_mode: Option<&str>,
) -> Result<ParsedScript, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("脚本内容为空，请选择音源脚本文件或粘贴脚本内容".into());
    }
    // 1) 明显的非脚本文档（误选了网页/数据文件）
    if let Some(kind) = document_kind(trimmed) {
        return Err(format!(
            "文件内容是{kind}，不是 JavaScript 音源脚本。请确认选择的是音源脚本文件（如 source.js）"
        ));
    }
    // 2) 结构合法性：命中任意一项音源脚本特征即可（不要求 LX 专属写法）
    if !has_source_marker(trimmed) {
        return Err(
            "文件结构不像音源脚本：未发现取链方法（如 musicUrl）、音源接口路径（如 url.php）、\
             平台声明（如 wy / tx / kg）或接口地址常量（如 API_BASE）。\
             请确认选择的是音源脚本文件（如 source.js）"
                .into(),
        );
    }

    // 3) 取链基址：前端已执行脚本取出则直接用；否则静态解析
    let base = match hint_base {
        Some(b) if !b.trim().is_empty() => normalize_base(b.trim())
            .ok_or_else(|| {
                "前端回填的接口基址格式无效，请确认脚本内包含正确的接口地址".to_string()
            })?,
        _ => extract_base(trimmed).ok_or_else(|| {
            "无法从脚本中解析出取链接口地址（API_BASE）。请确认脚本内包含接口基址\
             （如 const API_BASE = 'https://…'）；含加密运算或私有协议的脚本暂不支持"
                .to_string()
        })?,
    };
    let api_mode = hint_mode.unwrap_or("").to_string();

    // 脚本名：@name 元数据 → 取链域名。脚本内中文名多为 \uXXXX 转义写法，需解码
    let name = extract_script_name(trimmed)
        .map(|n| decode_js_escapes(&n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("{} 音源", host_of(&base)));
    let platforms = extract_platforms(trimmed);

    Ok(ParsedScript { name, base_url: base, platforms, api_mode })
}

/// 非脚本文档识别：误选网页 / JSON / XML 数据文件时给出准确提示。
/// 只判断"明显不是 JS 脚本"的情形，脚本本身不受影响。
fn document_kind(text: &str) -> Option<&'static str> {
    let head_end = text.ceil_char_boundary(512.min(text.len()));
    let head = text[..head_end].to_lowercase();
    let head = head.trim_start();
    if head.starts_with("<!doctype") || head.starts_with("<html") {
        return Some("HTML 网页");
    }
    if head.starts_with("<?xml") {
        return Some("XML 数据");
    }
    // JSON：以 { 或 [ 开头且整体可被解析（脚本不会满足这两点）
    if (text.starts_with('{') || text.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(text).is_ok()
    {
        return Some("JSON 数据");
    }
    None
}

/// 音源脚本的结构特征判定：命中任意一项即认为是结构合法的音源脚本。
///
/// 刻意做成"宽进"：`globalThis.lx` 与 `musicUrl` 都只作为**可选特征**参与判定，
/// 不再作为强制条件，因此普通音源脚本（自有函数命名、模块导出等写法）也能导入。
fn has_source_marker(text: &str) -> bool {
    let lower = text.to_lowercase();

    // 1) 取链方法 / 动作名（LX 的 musicUrl 动作，或常见的函数命名风格）
    for m in [
        "musicurl",
        "getmusicurl",
        "getplayurl",
        "getaudiostream",
        "songurl",
        "playurl",
        "geturl",
    ] {
        if lower.contains(m) {
            return true;
        }
    }
    // 2) 音源接口路径（lx-online-api 系部署）
    for p in ["url.php", "lyric.php", "pic.php", "search.php", "platforms.php"] {
        if lower.contains(p) {
            return true;
        }
    }
    // 3) 平台 / 音质声明：`wy: { name: …, qualitys: […] }` 或 `sources = { … }`
    if platform_re().is_match(text)
        || (lower.contains("sources")
            && (lower.contains("qualitys") || lower.contains("qualities")))
    {
        return true;
    }
    // 4) 接口地址常量（API_BASE / BASE_URL …）
    if has_base_const(text) {
        return true;
    }
    // 5) LX 宿主对象（lx-music 系脚本）—— 仅作识别，不作为强制条件
    lower.contains("globalthis.lx")
        || lower.contains("globalthis[\"lx\"]")
        || lower.contains("globalthis['lx']")
        || has_lx_server_signature(&lower)
}

/// LX 服务端下发（打包 / 混淆）音源脚本签名识别。
///
/// 洛雪 / 落雪系服务器推送脚本（如 `lx-music-source-v6+`）的主体经过 JS
/// Packer 混淆：取链方法（`musicUrl`）、接口路径（`url.php`）、平台声明（`wy:`）
/// 都只在运行时解包后才存在，静态文本里一个都看不到；它也不引用 `globalThis.lx`。
/// 但它必定带 `globalThis['SERVER_SCRIPT_CONFIG']`，里面含 `apiUrl`（即取链接口
/// 基址）。用「下发配置签名」识别这类脚本，避免被误判成「非音源脚本」。
fn has_lx_server_signature(lower: &str) -> bool {
    lower.contains("server_script_config") && lower.contains("apiurl")
}

/// 是否声明了接口基址常量（不含 http 字面量回退，避免任意 URL 都能命中）
fn has_base_const(text: &str) -> bool {
    Regex::new(
        r#"(?i)(?:const|let|var)\s+(?:API_BASE|API_HOST|BASE_URL|BASEURL|APIBASE|API_URL)\s*="#,
    )
    .map(|re| re.is_match(text))
    .unwrap_or(false)
}

/// 平台声明块正则（`wy: { … }`），解析与结构判定共用同一份，避免两处漂移。
fn platform_re() -> Regex {
    Regex::new(r#"(?m)^\s*(wy|tx|kw|kg|mg|joox|xm|bd)\s*:\s*\{([^{}]*)\}"#).expect("platform regex")
}

/// 提取 API_BASE：优先 `const API_BASE = '…'`（兼容常见变量名与引号风格），
/// 回退到脚本内第一个出现的 https(s):// 域名字面量。
fn extract_base(text: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?i)(?:const|let|var)\s+(?:API_BASE|API_HOST|BASE_URL|BASEURL|APIBASE|API_URL|apiBase|baseUrl)\s*=\s*['"]([^'"]+)['"]"#,
    )
    .ok()?;
    if let Some(cap) = re.captures(text) {
        let candidate = normalize_base(&cap[1]);
        if candidate.is_some() {
            return candidate;
        }
    }
    // apiUrl：LX 服务端下发脚本把取链基址放在 SERVER_SCRIPT_CONFIG 的 JSON 里
    //（`"apiUrl":"https:\/\/host"`，斜杠被转义），也可能写成 `apiUrl = 'https://…'`。
    // 这类脚本没有 API_BASE 常量，必须单独抽取。JSON 写法里 apiUrl 是带引号键名，
    // 键名与冒号之间还有个闭合引号，故用 ["']? 兼容。
    let re_api = Regex::new(r#"(?i)apiurl["']?\s*[:=]\s*['"]([^'"]+)['"]"#).ok()?;
    if let Some(cap) = re_api.captures(text) {
        let candidate = normalize_base(&cap[1]);
        if candidate.is_some() {
            return candidate;
        }
    }
    // 回退：脚本里第一个 http(s) 字面量（排除明显是文档/说明的链接）
    let re_url = Regex::new(r#"['"](https?://[^'"\s]+)['"]"#).ok()?;
    for cap in re_url.captures_iter(text) {
        let candidate = normalize_base(&cap[1]);
        if candidate.is_some() {
            return candidate;
        }
    }
    None
}

/// 归一化基址：去尾斜杠、去查询串；必须 http(s) 且含主机名。
fn normalize_base(raw: &str) -> Option<String> {
    // JSON 字符串里 URL 常被写成 "https:\/\/host"（斜杠被转义），先还原
    let mut s = raw.trim().replace("\\/", "/").to_string();
    if s.starts_with("http://") || s.starts_with("https://") {
        if let Some(pos) = s.find('?') {
            s.truncate(pos);
        }
        while s.ends_with('/') {
            s.pop();
        }
        let host_ok = s
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .map(|h| h.contains('.'))
            .unwrap_or(false);
        if host_ok {
            return Some(s);
        }
    }
    None
}

/// 提取脚本头部 `@name` 元数据
fn extract_script_name(text: &str) -> Option<String> {
    let re = Regex::new(r"@name\s+([^\r\n*]+)").ok()?;
    re.captures(text)
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 提取 sources 声明里的平台与音质：
/// 形如 `wy: { name: '网易云音乐', qualitys: ['128k', '320k', 'flac'] }`
/// （兼容双引号、无 name、`qualities`/`qualitys` 两种拼写、跨行数组）
fn extract_platforms(text: &str) -> Vec<LxPlatform> {
    let re = platform_re();
    let re_name = Regex::new(r#"(?i)\bname\s*:\s*['"]([^'"]*)['"]"#).expect("name regex");
    // 注意：既支持官方拼写 qualitys 也支持误写 qualities（qualit + y|ie + s）
    let re_quals =
        Regex::new(r#"(?i)\bqualit(?:y|ie)s?\s*:\s*\[([^\]]*)\]"#).expect("qualities regex");
    let re_actions =
        Regex::new(r#"(?i)\bactions\s*:\s*\[([^\]]*)\]"#).expect("actions regex");

    let mut out: Vec<LxPlatform> = Vec::new();
    for cap in re.captures_iter(text) {
        let code = cap[1].to_string();
        let body = &cap[2];
        let name = re_name
            .captures(body)
            .map(|c| decode_js_escapes(c[1].trim()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| platform_name(&code));
        let qualitys = re_quals.captures(body).map(|c| split_list(&c[1]));
        let actions = re_actions
            .captures(body)
            .map(|c| split_list(&c[1]))
            .unwrap_or_else(|| vec!["musicUrl".into()]);
        let qualitys = qualitys.unwrap_or_default();
        out.push(LxPlatform { code, name, actions, qualitys });
    }
    out
}

/// 解码 JS 字符串转义：`\uXXXX`（含大写十六进制）→ Unicode 字符。
/// 音源脚本里中文常用 `'\u7f51\u6613...'` 写法，不解码列表里会显示原始转义序列。
fn decode_js_escapes(s: &str) -> String {
    if !s.contains("\\u") {
        return s.to_string();
    }
    let re = Regex::new(r"\\u\{?([0-9a-fA-F]{4})\}?").expect("unicode escape regex");
    re.replace_all(s, |caps: &regex::Captures| {
        u32::from_str_radix(&caps[1], 16)
            .ok()
            .and_then(char::from_u32)
            .map(|c| c.to_string())
            .unwrap_or_else(|| caps[0].to_string())
    })
    .into_owned()
}

/// 拆 `['128k', "320k", flac]` 这类 JS 数组字面量为干净字符串列表
fn split_list(body: &str) -> Vec<String> {
    body.split(',')
        .map(|seg| {
            seg.trim()
                .trim_matches(|c| c == '\'' || c == '"' || c == ' ')
        })
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// ---------- HTTP ----------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(TIMEOUT_CONNECT)
        .timeout_read(TIMEOUT_READ)
        .build()
}

/// 把网络错误翻译成带场景的中文提示（超时 / DNS / 状态码）。
/// ureq 的 TransportKind Display 为小写（"dns error: …" / "io error: …timed out…"），
/// 统一小写后匹配，避免大小写假设。
fn http_err(context: &str, e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("{context}: 服务器返回 HTTP {code}"),
        ureq::Error::Transport(t) => {
            let kind = t.kind().to_string().to_lowercase();
            if kind.starts_with("dns") {
                format!("{context}: 域名无法解析，请检查地址拼写与网络连接")
            } else if kind.contains("timed out") || kind.contains("timeout") {
                format!(
                    "{context}: 请求超时（{}s 连接 / {}s 读取），服务器无响应或网络不通",
                    TIMEOUT_CONNECT.as_secs(),
                    TIMEOUT_READ.as_secs()
                )
            } else if kind.starts_with("url") {
                format!("{context}: 地址格式无效")
            } else {
                format!("{context}: 网络错误（{kind}）")
            }
        }
    }
}

/// 统一的 JSON GET：非 2xx / 非正文都会给出场景化错误
fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let resp = agent()
        .get(url)
        .set("User-Agent", UA)
        .call()
        .map_err(|e| http_err("请求失败", e))?;
    resp.into_json::<serde_json::Value>()
        .map_err(|e| format!("响应不是合法 JSON: {e}"))
}

/// LX 接口约定：`{code:0, data:…}` 为成功；否则取 `msg` 作为错误
fn unwrap_lx_envelope(v: serde_json::Value, what: &str) -> Result<serde_json::Value, String> {
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = v
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(format!("{what}失败: {msg}"));
    }
    v.get("data")
        .cloned()
        .ok_or_else(|| format!("{what}失败: 响应缺少 data 字段"))
}

// ---------- 探测（添加网络源时校验） ----------

/// 探测结果
pub struct ProbeOutcome {
    pub name: String,
    pub base_url: String,
    pub platforms: Vec<LxPlatform>,
    /// 命中的探测方式（UI 展示 / 错误提示用）
    pub via: String,
    /// 脚本订阅时携带的脚本文本（入库保留原文，避免二次拉取）
    pub origin: String,
}

/// 添加网络音源：拉取并校验。
///
/// 三段式探测（任一通过即接受，越靠前信息越全）：
/// 1. URL 以 .js 结尾 → 按音源脚本订阅拉取 + 静态解析；
/// 2. `{base}/platforms.php` → 平台契约（最强校验）；
/// 3. `{base}/url.php` 空参试探 → 只要返回 LX 风格 JSON 即认为接口存在（弱校验）。
pub fn probe_source(url: &str) -> Result<ProbeOutcome, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("地址不能为空".into());
    }
    // 订阅脚本：直接拉取解析（原文随结果带回，调用方入库不再二次拉取）
    let lower = trimmed.split('?').next().unwrap_or("").to_lowercase();
    if lower.ends_with(".js") {
        let text = http_get_text(trimmed)?;
        let parsed = parse_script(&text).map_err(|e| format!("订阅脚本校验失败: {e}"))?;
        return Ok(ProbeOutcome {
            name: parsed.name,
            base_url: parsed.base_url,
            platforms: parsed.platforms,
            via: "音源脚本订阅".into(),
            origin: text,
        });
    }

    let base = normalize_base(trimmed)
        .ok_or("地址格式无效：需要 http(s):// 开头的完整地址（示例 https://music.example.com）")?;

    // 1) platforms.php
    match get_json(&format!("{base}/platforms.php")) {
        Ok(v) => {
            if let Ok(data) = unwrap_lx_envelope(v, "平台列表") {
                let platforms = parse_platforms_php(&data);
                if !platforms.is_empty() {
                    return Ok(ProbeOutcome {
                        name: host_of(&base),
                        base_url: base,
                        platforms,
                        via: "platforms.php".into(),
                        origin: String::new(),
                    });
                }
            }
        }
        Err(e) => eprintln!("[lxsource] platforms.php 探测: {e}"),
    }

    // 2) url.php 弱探测（参数故意给空 id：只要接口存在，无论业务成败都返回 JSON）
    match get_json(&format!("{base}/url.php?source=wy&id=0&quality=128k")) {
        Ok(v) => {
            if v.get("code").is_some() {
                return Ok(ProbeOutcome {
                    name: host_of(&base),
                    base_url: base,
                    platforms: vec![],
                    via: "url.php 接口探测".into(),
                    origin: String::new(),
                });
            }
            Err("该地址可访问但不是音源接口（url.php 响应缺少 code 字段）。\
                 请填写音源站根地址（如 https://music.example.com）或音源脚本链接"
                .into())
        }
        Err(e) => Err(format!(
            "音源校验失败：{e}。已尝试 platforms.php 与 url.php，均不可达；\
             请确认地址正确、服务器在线，且为 lx-online-api 类音源站"
        )),
    }
}

fn http_get_text(url: &str) -> Result<String, String> {
    let resp = agent()
        .get(url)
        .set("User-Agent", UA)
        .call()
        .map_err(|e| http_err("拉取失败", e))?;
    resp.into_string()
        .map_err(|e| format!("读取响应失败: {e}"))
}

/// platforms.php 的 data.platforms[] → 契约（该部署字段名为 qualities）
fn parse_platforms_php(data: &serde_json::Value) -> Vec<LxPlatform> {
    let empty = Vec::new();
    let arr = data
        .get("platforms")
        .and_then(|p| p.as_array())
        .unwrap_or(&empty);
    arr.iter()
        .filter_map(|p| {
            let code = p.get("code")?.as_str()?.to_string();
            let name = p
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let qualitys: Vec<String> = p
                .get("qualities")
                .or_else(|| p.get("qualitys"))
                .and_then(|q| q.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let actions: Vec<String> = p
                .get("actions")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_else(|| vec!["musicUrl".into()]);
            Some(LxPlatform {
                code: code.clone(),
                name: if name.is_empty() { platform_name(&code) } else { name },
                actions,
                qualitys,
            })
        })
        .collect()
}

fn host_of(base: &str) -> String {
    base.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(base)
        .to_string()
}

// ---------- 取链 ----------

/// 取音频直链。
///
/// 按 `api_mode` 分流两种协议：
/// - `""`（标准 LX）：`GET {base}/url.php?source&id&quality[&extra]` → data.url
/// - `"v1"`（自定义 NestJS）：`POST {base}/v1/music/resolve-url`，DTO 为
///   `{rid, level, source}`，返回标准 LX 信封 `{code:0, data:{url}}`，无需签名。
///   混淆脚本（基址运行时解码）即走此分支。
pub fn music_url(
    base: &str,
    source: &str,
    id: &str,
    quality: &str,
    extra: Option<&str>,
    api_mode: &str,
) -> Result<String, String> {
    if id.is_empty() || id == "0" {
        return Err("歌曲 ID 无效，无法取链".into());
    }
    if api_mode == "v1" {
        return music_url_v1(base, source, id, quality);
    }
    let mut url = format!("{base}/url.php?source={source}&id={id}&quality={quality}");
    if let Some(ex) = extra {
        if !ex.is_empty() {
            url.push_str("&extra=");
            url.push_str(&percent_encode(ex));
        }
    }
    let data = unwrap_lx_envelope(get_json(&url)?, "取链")?;
    let link = data
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    if !link.starts_with("http://") && !link.starts_with("https://") {
        return Err("取链失败: 接口返回的直链无效".into());
    }
    Ok(link)
}

/// 自定义 v1 端点取链（`POST {base}/v1/music/resolve-url`）。
///
/// 与标准 LX 协议共用同一份 `{code:0, data:{url}}` 信封，区别仅在请求方式：
/// 标准协议把参数放进 query，v1 放进 JSON body，字段名为 `rid`/`level`/`source`。
/// 该端点常见于洛雪系"二次修改"聚合音源（基址运行时解码、无标准 lyric.php）。
fn music_url_v1(base: &str, source: &str, id: &str, quality: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "rid": id,
        "level": quality,
        "source": source,
    });
    let resp = agent()
        .post(&format!("{base}/v1/music/resolve-url"))
        .set("User-Agent", UA)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| http_err("取链", e))?;
    let v = resp
        .into_json::<serde_json::Value>()
        .map_err(|e| format!("取链响应不是合法 JSON: {e}"))?;
    let data = unwrap_lx_envelope(v, "取链")?;
    let link = data
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    if !link.starts_with("http://") && !link.starts_with("https://") {
        return Err("取链失败: 接口返回的直链无效".into());
    }
    Ok(link)
}

/// 极简 percent-encode（extra 是 JSON 文本，含 {}"/ 等需编码字符）
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 取歌词：`GET {base}/lyric.php?source=&id=` → `{code:0,data:{lyric[,tlyric]}}`。
///
/// 与 `music_url` 同协议（lx-online-api 系）；优先取 `lyric`，为空时回退 `tlyric`
/// （翻译）。返回原始 LRC 文本，由 `lyrics` 模块解析成逐字 / 行级结构。
pub fn lyric(base: &str, source: &str, id: &str) -> Result<String, String> {
    if id.is_empty() || id == "0" {
        return Err("歌曲 ID 无效，无法获取歌词".into());
    }
    let url = format!("{base}/lyric.php?source={source}&id={id}");
    let data = unwrap_lx_envelope(get_json(&url)?, "歌词")?;
    let lyric = data
        .get("lyric")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !lyric.is_empty() {
        return Ok(lyric);
    }
    // 部分部署把翻译放在 tlyric（同样为 LRC），原文缺失时回退使用
    let tl = data
        .get("tlyric")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !tl.is_empty() {
        return Ok(tl);
    }
    Err("该音源未返回歌词".into())
}

// ---------- 搜索（排行榜） ----------

/// 音源搜索：`GET {base}/search.php?source=&keyword=&limit=&page=`
///
/// 并非所有部署都提供搜索接口（LX 脚本常见契约只有 musicUrl / lyric / pic），
/// 调用方需处理"接口不存在"的失败并回退到内置平台搜索。
/// 字段命名在各部署间差异较大，这里做宽松解析：任一首缺 id 或标题即丢弃。
pub fn search_songs(
    base: &str,
    source: &str,
    keyword: &str,
    limit: i64,
) -> Result<Vec<LxSearchSong>, String> {
    let url = format!(
        "{base}/search.php?source={source}&keyword={}&limit={limit}&page=1",
        percent_encode(keyword)
    );
    let data = unwrap_lx_envelope(get_json(&url)?, "搜索")?;
    let empty = Vec::new();
    let arr = data
        .get("list")
        .and_then(|l| l.as_array())
        .or_else(|| data.get("songs").and_then(|l| l.as_array()))
        .or_else(|| data.get("data").and_then(|l| l.as_array()))
        .or_else(|| data.as_array())
        .unwrap_or(&empty);
    Ok(arr
        .iter()
        .map(parse_song)
        .filter(|s| !s.id.is_empty() && !s.title.is_empty())
        .collect())
}

/// 从搜索结果的一个条目解析统一歌曲结构（字段别名兼容多套部署）
fn parse_song(v: &serde_json::Value) -> LxSearchSong {
    LxSearchSong {
        id: first_str(v, &["id", "songmid", "songId", "mid", "hash", "rid"])
            .unwrap_or_default(),
        title: first_str(v, &["name", "songname", "songName", "title"]).unwrap_or_default(),
        artist: artists_str(v),
        album: album_str(v),
        duration_ms: duration_ms_of(v),
        platform: String::new(),
        // 酷狗 hash / QQ media_mid：取链时可能需要原样透传
        extra: first_str(v, &["hash", "media_mid", "mediaMid", "strMediaMid"])
            .unwrap_or_default(),
    }
}

/// 取第一个存在且非空的字符串字段（数字/布尔按字符串化）
fn first_str(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(x) = v.get(*k) {
            match x {
                serde_json::Value::String(s) => {
                    if !s.trim().is_empty() {
                        return Some(s.trim().to_string());
                    }
                }
                serde_json::Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

/// 歌手：可能是字符串、`[{name}]` 或 `["a","b"]`
fn artists_str(v: &serde_json::Value) -> String {
    for k in ["artist", "artists", "singer", "singers", "author", "authors"] {
        if let Some(x) = v.get(k) {
            match x {
                serde_json::Value::String(s) => return s.trim().to_string(),
                serde_json::Value::Array(a) => {
                    let names: Vec<String> = a
                        .iter()
                        .filter_map(|e| match e {
                            serde_json::Value::String(s) => Some(s.trim().to_string()),
                            serde_json::Value::Object(_) => {
                                first_str(e, &["name", "title", "singer"])
                            }
                            _ => None,
                        })
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !names.is_empty() {
                        return names.join(" / ");
                    }
                }
                _ => {}
            }
        }
    }
    String::new()
}

/// 专辑：可能是字符串或 `{name}`
fn album_str(v: &serde_json::Value) -> String {
    for k in ["album", "albumName", "albumname", "album_title", "albumTitle"] {
        if let Some(x) = v.get(k) {
            match x {
                serde_json::Value::String(s) => return s.trim().to_string(),
                serde_json::Value::Object(_) => {
                    if let Some(n) = first_str(x, &["name", "title"]) {
                        return n;
                    }
                }
                _ => {}
            }
        }
    }
    String::new()
}

/// 时长：字段可能给秒也可能给毫秒（秒值恒 < 1000），统一成毫秒
fn duration_ms_of(v: &serde_json::Value) -> u64 {
    let raw = v
        .get("durationMs")
        .or_else(|| v.get("duration_ms"))
        .and_then(|x| x.as_u64())
        .map(|ms| (ms, true))
        .or_else(|| {
            v.get("duration")
                .or_else(|| v.get("interval"))
                .or_else(|| v.get("time"))
                .and_then(|x| x.as_u64())
                .map(|n| (n, false))
        });
    match raw {
        Some((n, true)) => n,
        // 小于 1000 视为秒（常规曲目 100–600 秒），否则按毫秒
        Some((n, false)) if n < 1000 => n * 1000,
        Some((n, false)) => n,
        None => 0,
    }
}

// ---------- 测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

const SAMPLE: &str = r#"
/*!
 * LX Online Source API (PHP shared-host build, ready to use)
 */
// API endpoint base = where url.php / lyric.php / pic.php live
const API_BASE = 'https://music.example.com'

const { EVENT_NAMES, request, on, send, utils } = globalThis.lx

const SOURCES = {
  wy: { name: '\u7f51\u6613\u4e91\u97f3\u4e50', qualitys: ['128k', '192k', '320k', 'flac', 'flac24bit', 'hires', 'atmos', 'master'] },
  tx: { name: 'QQ音乐', qualitys: ['128k', '192k', '320k', 'flac', 'flac24bit', 'hires', 'atmos', 'atmos_plus', 'master'] },
  kw: { name: '酷我音乐', qualitys: ['128k', '192k', '320k', 'flac', 'flac24bit'] },
}

on(EVENT_NAMES.request, ({ action, source, info }) => {
  switch (action) {
    case 'musicUrl':
      return httpGet('/url.php?' + qs + '&quality=' + (info.type || '320k')).then((data) => data.url)
    case 'lyric':
      return httpGet('/lyric.php?' + qs)
    case 'pic':
      return httpGet('/pic.php?' + qs).then((data) => data.url)
  }
})

send(EVENT_NAMES.inited, { status: true, openDevTools: false, sources })
"#;

#[test]
fn parses_sample_script() {
    let p = parse_script(SAMPLE).expect("parse ok");
    assert_eq!(p.base_url, "https://music.example.com");
    // 无 @name 头 → 回退「域名 音源」
    assert_eq!(p.name, "music.example.com 音源");
    assert_eq!(p.platforms.len(), 3);
    let wy = p.platforms.iter().find(|x| x.code == "wy").unwrap();
    // 脚本内中文为 \uXXXX 转义写法，必须解码为汉字
    assert_eq!(wy.name, "网易云音乐");
    assert!(wy.qualitys.contains(&"flac24bit".to_string()));
    assert!(wy.qualitys.contains(&"master".to_string()));
}

#[test]
fn decodes_js_unicode_escapes() {
    assert_eq!(decode_js_escapes(r"\u7f51\u6613"), "网易");
    assert_eq!(decode_js_escapes("plain"), "plain");
}

    /// 普通音源脚本（不引用 globalThis.lx）也必须能导入成功
    const PLAIN: &str = r#"
// 自建音源：只导出取链方法，不依赖任何宿主对象
const API_BASE = 'https://music.example.com'

export const sources = {
  kg: { name: '酷狗音乐', qualitys: ['128k', '320k'] },
}

export async function getMusicUrl(songInfo) {
  const r = await fetch(`${API_BASE}/url.php?source=kg&id=${songInfo.id}&quality=320k`)
  return (await r.json()).data.url
}
"#;

    /// 只声明平台与接口常量、函数命名非 musicUrl 的极简脚本，同样应放行
    const MINIMAL: &str = r#"
const BASE_URL = 'https://music.example.com'
const PLATFORMS = [
  wy: { name: '网易云音乐' },
]
"#;

    #[test]
    fn lx_script_import_unchanged() {
        // 含 globalThis.lx 的 LX 脚本：行为与改动前一致
        let p = parse_script(SAMPLE).expect("LX 脚本应通过");
        assert_eq!(p.base_url, "https://music.example.com");
    }

    #[test]
    fn accepts_plain_source_script() {
        // 无 globalThis.lx 的普通音源脚本：允许导入（本次改动的重点）
        let p = parse_script(PLAIN).expect("普通脚本应通过");
        assert_eq!(p.base_url, "https://music.example.com");
        assert_eq!(p.platforms.len(), 1);
        assert_eq!(p.platforms[0].code, "kg");
        assert_eq!(p.platforms[0].name, "酷狗音乐");
    }

    #[test]
    fn accepts_minimal_script() {
        // 只有接口常量 + 平台声明，函数命名不含 musicUrl：仍应放行
        let p = parse_script(MINIMAL).expect("极简脚本应通过");
        assert_eq!(p.base_url, "https://music.example.com");
        assert_eq!(p.platforms.len(), 1);
    }

    #[test]
    fn rejects_non_source_script() {
        // 普通 JS 文件：无音源脚本特征 → 结构不合法
        let err = parse_script("console.log('hi')").unwrap_err();
        assert!(err.contains("不像音源脚本"), "{err}");
        assert!(!err.contains("LX"), "{err}");
    }

    #[test]
    fn rejects_html_document() {
        // 误选网页：给出"不是脚本"而非"不是 LX 脚本"
        let err = parse_script("<!doctype html><html><body>hi</body></html>").unwrap_err();
        assert!(err.contains("HTML 网页"), "{err}");
    }

    #[test]
    fn rejects_json_document() {
        let err = parse_script(r#"{"code":0,"data":{"url":"x"}}"#).unwrap_err();
        assert!(err.contains("JSON 数据"), "{err}");
    }

    #[test]
    fn rejects_no_base_url() {
        // 有音源特征但解析不出接口地址 → 保留"无法解析"的报错
        let err = parse_script("function musicUrl() { return buildLocalPath() }").unwrap_err();
        assert!(err.contains("无法从脚本中解析出取链接口地址"), "{err}");
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_script("   ").is_err());
    }

    /// 洛雪 / 落雪系「服务端下发」脚本：主体经 JS Packer 混淆，静态文本里
    /// 看不到 musicUrl / url.php / wy: 等标记，也不引用 globalThis.lx，只带
    /// `SERVER_SCRIPT_CONFIG`（内含 apiUrl）。此前会被误判成「非音源脚本」。
    const SERVER_SCRIPT: &str = r#"
// 服务端下发配置（自动生成，请勿修改）
globalThis['SERVER_SCRIPT_CONFIG'] = {"apiUrl":"https:\/\/88.lxmusic.xn--fiqs8s","apiKey":"lxmusic","signSalt":"LxSrv@2026#Sig","fingerprint":"ffdaccdf66796c1cbe96df07bf682118"};
// ===== 服务端下发配置结束 =====
;(function(p,a,c,k,e,d){e=function(c){return c};/* packed body … */})();
"#;

    #[test]
    fn accepts_lx_server_script() {
        // 混淆的服务端脚本：应接受，并从容器的 apiUrl 抽出取链基址
        let p = parse_script(SERVER_SCRIPT).expect("服务端下发脚本应通过");
        assert_eq!(p.base_url, "https://88.lxmusic.xn--fiqs8s");
    }

    #[test]
    fn server_script_signature_does_not_false_accept_random() {
        // 随机普通 JS 文件：既无 SERVER_SCRIPT_CONFIG 也无其它音源特征 → 仍应拒绝
        let random = parse_script("function fetchData() { return 1 }").unwrap_err();
        assert!(random.contains("不像音源脚本"), "{random}");
    }

    #[test]
    fn normalize_base_unescapes_slashes() {
        // JSON 里的 "https:\/\/host" 斜杠被转义，必须还原成 "https://host"
        assert_eq!(
            normalize_base("https:\\/\\/88.lxmusic.xn--fiqs8s"),
            Some("https://88.lxmusic.xn--fiqs8s".into())
        );
    }

    #[test]
    fn normalize_base_variants() {
        assert_eq!(
            normalize_base("https://music.example.com/"),
            Some("https://music.example.com".into())
        );
        assert_eq!(normalize_base("ftp://x"), None);
        assert_eq!(normalize_base("https://localhost"), None);
    }

    #[test]
    fn percent_encode_extra() {
        assert_eq!(percent_encode(r#"{"hash":"a/b"}"#), r#"%7B%22hash%22%3A%22a%2Fb%22%7D"#);
    }

    /// 搜索结果的字段别名兼容：songmid/songname/singer[{name}]/album{name}/interval(秒)
    #[test]
    fn parses_search_song_aliases() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"songmid":"003aBcD1","songname":"晴天","singer":[{"name":"周杰伦"}],
                "album":{"name":"叶惠美"},"interval":269}"#,
        )
        .unwrap();
        let s = parse_song(&v);
        assert_eq!(s.id, "003aBcD1");
        assert_eq!(s.title, "晴天");
        assert_eq!(s.artist, "周杰伦");
        assert_eq!(s.album, "叶惠美");
        // interval 给的是秒 → 转毫秒
        assert_eq!(s.duration_ms, 269_000);
    }

    /// 时长字段给出毫秒时不要二次换算
    #[test]
    fn parses_search_song_duration_ms() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"id":"7","name":"a","artist":"b","durationMs":269000}"#)
                .unwrap();
        assert_eq!(parse_song(&v).duration_ms, 269_000);
    }
}
