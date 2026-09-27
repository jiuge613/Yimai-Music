// 复测：对候选镜像跑 2 轮，并打印真实错误原因。
// 仅 https，且拒绝 localhost/环回/私有/保留地址。
import { readFileSync } from "node:fs";

const REPO = "jiuge613/Yimai-Music";
const TAG = "v1.1.1";
const ASSET = "YimaiMusic_1.1.1.0_x64-setup.exe";
const GH = `https://github.com/${REPO}/releases/download/${TAG}/${ASSET}`;
const BYTES = 3 * 1024 * 1024;

const CANDIDATES = [
  "https://ghfile.geekertao.top/",
  "https://gh-proxy.com/",
  "https://github.chenc.dev/",
  "https://gh.kichills.cn/",
  "https://cdn.akacoder.online/",
  "https://gh.ddlc.top/",
  "https://ghproxy.cc/",
  "https://ghproxy.mnjiang.cn/",
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

async function once(url) {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), 15000);
  const t0 = Date.now();
  try {
    const r = await fetch(url, { signal: ac.signal, headers: { "User-Agent": "YimaiMusic/1.1" }, redirect: "follow" });
    if (!r.ok) { clearTimeout(timer); return { ok: false, err: "HTTP " + r.status }; }
    const reader = r.body.getReader();
    let got = 0;
    while (got < BYTES) { const { done, value } = await reader.read(); if (done) break; got += value.length; }
    clearTimeout(timer);
    const s = (Date.now() - t0) / 1000;
    return { ok: true, mb: +(got / 1048576).toFixed(2), s: +s.toFixed(2), mbps: +((got / 1048576) / s).toFixed(2) };
  } catch (e) {
    clearTimeout(timer);
    // Node fetch 把底层原因藏在 cause 里
    const cause = e && e.cause ? `${e.cause.code || e.cause.message || ""}` : "";
    return { ok: false, err: `${e.name || e.message}${cause ? " / " + cause : ""}`.slice(0, 70) };
  }
}

console.log("== 直连 GitHub（2 轮）==");
for (let i = 0; i < 2; i++) console.log("  ", JSON.stringify(await once(GH)));

console.log("\n== 候选镜像（各 2 轮）==");
const rows = [];
for (const m of CANDIDATES) {
  const label = m.replace(/^https:\/\//, "").replace(/\/$/, "");
  if (!hostOk(m)) { console.log(`  ${label}  host 未通过安全校验`); continue; }
  const a = await once(m + GH);
  const b = await once(m + GH);
  const best = a.ok && b.ok ? Math.max(a.mbps, b.mbps) : (a.ok ? a.mbps : b.ok ? b.mbps : null);
  rows.push({ label, a, b, best });
  console.log(`  ${label.padEnd(32)} r1=${a.ok ? a.mbps + "MB/s" : a.err}  r2=${b.ok ? b.mbps + "MB/s" : b.err}`);
}
console.log("\n== 结论（按最好一轮排序）==");
for (const r of rows.filter((x) => x.best).sort((x, y) => y.best - x.best)) console.log(`  ${String(r.best).padStart(6)} MB/s  ${r.label}`);
