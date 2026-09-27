// 校验打包产物里确实带上了新的文案，且旧文案已消失。
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const dir = fileURLToPath(new URL("../dist/assets/", import.meta.url));
// Vite 会把代码拆成多个 chunk（index/main/window/...），要合并后再查，
// 否则只看 index-*.js 会漏掉真正的业务代码。
const s = readdirSync(dir)
  .filter((n) => n.endsWith(".js"))
  .map((n) => readFileSync(dir + n, "utf8"))
  .join("\n");

// [关键字, 说明, 期望存在?]
const checks = [
  ["Yimai Music", "品牌名", true],
  ["FLAC/WAV", "无损文案", true],
  ["中档", "中档音质", true],
  ["高音质", "高音质档", true],
  ["256K", "较高 256K", true],
  ["192K", "中档 192K", true],
  ["本地音乐", "本地音乐", true],
  ["移出本地音乐", "移出按钮", true],
  ["资料库", "旧「资料库」(应已清空)", false],
  ["已下载到资料库", "旧下载 toast (应已清空)", false],
  ["下载歌曲到资料库", "旧下载提示 (应已清空)", false],
];

let bad = 0;
for (const [key, label, want] of checks) {
  const has = s.includes(key);
  const ok = has === want;
  if (!ok) bad++;
  console.log(`[${ok ? "ok  " : "FAIL"}] ${label.padEnd(22)} ${key}  → ${has ? "存在" : "不存在"}`);
}
console.log(`\ndist/assets/*.js: ${bad ? bad + " 项不符" : "全部符合"}`);
process.exit(bad ? 1 : 0);
