// 实测各 GitHub 代理镜像对「更新包」的下载速度。
// 只允许 https，且拒绝 localhost/环回/私有/保留地址（与运行时 SSRF 防护一致）。
import { readFileSync } from "node:fs";

const REPO = "jiuge613/Yimai-Music";
const TAG = "v1.1.1";
const ASSET = "YimaiMusic_1.1.1.0_x64-setup.exe";
const GH = `https://github.com/${REPO}/releases/download/${TAG}/${ASSET}`;
const BYTES = 3 * 1024 * 1024; // 取前 3MB 测吞吐
const TIMEOUT_MS = 12000;

const MIRRORS = [
  "https://ghproxy.cc/",
  "https://gh.monoliker.com/",
  "https://gproxy.twinzips.top/",
  "https://ghproxy.mnjiang.cn/",
  "https://ghproxy.mciel.com/",
  "https://github.chenc.dev/",
  "https://ghfile.geekertao.top/",
  "https://gh.llk-exm52bqpe.top/",
  "https://gh.kleyeas.com/",
  "https://ghm.0t8465.xyz/",
  "https://gh-proxy.com/",
  "https://github-proxy-memory-echoes.cn/",
  "https://fastgit.cc/",
  "https://gh.nokiu.com/",
  "https://gh.gxpk.top/",
  "https://gh.xcxxxo.cf/",
  "https://tv.tw/",
  "https://gh.kichills.cn/",
  "https://cdn.akacoder.online/",
  "https://ghfast.top/",
  "https://hub.gitmirror.com/",
  "https://gh.ddlc.top/",
  "https://ghproxy.homeboyc.cn/",
  "https://github.tbedu.top/",
  "https://gh.llkk.cc/",
  "https://ghproxy.cfd/",
];

// 与运行时一致的安全校验：仅 https + 拒绝内网/环回/保留地址
function hostOk(u) {
  let h;
  try { h = new URL(u).hostname.toLowerCase(); } catch { return false; }
  if (new URL(u).protocol !== "https:") return false;
  if (h === "localhost" || h.endsWith(".local") || h.endsWith(".internal")) return false;
  if (/^127\./.test(h) || /^10\./.test(h) || /^192\.168\./.test(h)) return false;
  if (/^172\.(1[6-9]|2\d|3[01])\./.test(h)) return false;
  if (/^169\.254\./.test(h) || /^0\./.test(h)) return false;
  return true;
}

async function bench(label, url) {
  if (!hostOk(url)) return { label, ok: false, why: "host 未通过安全校验" };
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), TIMEOUT_MS);
  const t0 = Date.now();
  try {
    const r = await fetch(url, { signal: ac.signal, headers: { "User-Agent": "YimaiMusic/1.1" }, redirect: "follow" });
    if (!r.ok) { clearTimeout(timer); return { label, ok: false, status: r.status }; }
    const reader = r.body.getReader();
    let got = 0;
    while (got < BYTES) {
      const { done, value } = await reader.read();
      if (done) break;
      got += value.length;
    }
    clearTimeout(timer);
    const secs = (Date.now() - t0) / 1000;
    return { label, ok: true, mb: +(got / 1048576).toFixed(2), secs: +secs.toFixed(2), mbps: +((got / 1048576) / secs).toFixed(2) };
  } catch (e) {
    clearTimeout(timer);
    return { label, ok: false, why: String(e.name || e.message).slice(0, 40) };
  }
}

const results = [];
results.push(await bench("直连 GitHub", GH));
for (const m of MIRRORS) results.push(await bench(m.replace(/\/$/, ""), m + GH));
results.sort((a, b) => (b.mbps ?? -1) - (a.mbps ?? -1));
const good = results.filter((r) => r.ok);
console.log("可用且速度排序：");
for (const r of good) console.log(`  ${String(r.mbps).padStart(6)} MB/s  ${r.mb}MB/${r.secs}s  ${r.label}`);
console.log("\n失败：");
for (const r of results.filter((x) => !x.ok)) console.log(`  ${r.label}  ${r.status ?? r.why}`);
