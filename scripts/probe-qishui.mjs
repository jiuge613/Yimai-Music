// 实测汽水音乐真实端点：哪些能用、哪些要登录/签名。
// 仅 https，且拒绝 localhost/环回/私有/保留地址。
const KW = encodeURIComponent("晴天 周杰伦");
const UA_APP =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36";

const CANDIDATES = [
  ["h5 seo 搜索", `https://music.douyin.com/qishui/so/${KW}`],
  ["h5 seo track", `https://music.douyin.com/qishui/track/${KW}`],
  ["douyin seo", `https://www.douyin.com/aweme/v1/web/hot/search/list/`],
  ["汽水 pc 搜索", `https://music.douyin.com/api/search/song/?keyword=${KW}&count=5`],
  ["qishui 首页", `https://music.douyin.com/qishui`],
  ["open api", `https://open.douyin.com/`],
];

function hostOk(u) {
  let url; try { url = new URL(u); } catch { return false; }
  if (url.protocol !== "https:") return false;
  const h = url.hostname.toLowerCase();
  if (h === "localhost" || h.endsWith(".local") || h.endsWith(".internal")) return false;
  if (/^(127\.|10\.|192\.168\.|0\.|169\.254\.)/.test(h)) return false;
  if (/^172\.(1[6-9]|2\d|3[01])\./.test(h)) return false;
  return true;
}

for (const [name, u] of CANDIDATES) {
  if (!hostOk(u)) { console.log(`[skip] ${name} host 未通过安全校验`); continue; }
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), 12000);
  try {
    const r = await fetch(u, {
      signal: ac.signal,
      headers: { "User-Agent": UA_APP, "Referer": "https://music.douyin.com/" },
      redirect: "follow",
    });
    clearTimeout(t);
    const ct = r.headers.get("content-type") || "";
    const body = ct.includes("json") ? (await r.text()).slice(0, 200) : `(非 JSON ${ct})`;
    console.log(`[${r.status}] ${name.padEnd(14)} ${r.url.slice(0, 70)}`);
    console.log(`        ${body.replace(/\s+/g, " ").slice(0, 180)}`);
  } catch (e) {
    clearTimeout(t);
    const c = e && e.cause ? e.cause.code || e.cause.message : "";
    console.log(`[ERR] ${name.padEnd(14)} ${e.name} ${c}`);
  }
}
