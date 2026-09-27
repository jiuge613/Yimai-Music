const UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36";
const u = "https://music.douyin.com/qishui/so/" + encodeURIComponent("晴天");
const r = await fetch(u, { headers: { "User-Agent": UA, Referer: "https://music.douyin.com/" } });
const t = await r.text();
console.log("状态", r.status, "长度", t.length, "类型", r.headers.get("content-type"));
for (const k of ["__INIT", "_SSR", "songList", "track_id", "play_url", "晴天", "mid", "id_str"]) {
  const n = (t.match(new RegExp(k, "g")) || []).length;
  console.log("  ", k.padEnd(10), "命中", n);
}
const i = t.indexOf("晴天");
console.log("样例:", i >= 0 ? t.slice(Math.max(0, i - 150), i + 150).replace(/\s+/g, " ") : "(未找到)");
