// 在 src/ 与 src-tauri/ 里按关键字搜索，避免 PowerShell 转义问题。
// 用法：node scripts/grep.mjs <正则> [子目录...]
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const pattern = new RegExp(process.argv[2], "i");
const EXT = /\.(ts|tsx|rs|json|css|html|md|toml)$/;
const root = fileURLToPath(new URL("..", import.meta.url));

const files = [];
(function walk(dir) {
  for (const e of readdirSync(dir)) {
    if (e === "node_modules" || e === "target" || e === "dist") continue;
    const p = join(dir, e);
    if (statSync(p).isDirectory()) walk(p);
    else if (EXT.test(e)) files.push(p);
  }
})(root);

let hits = 0;
for (const f of files) {
  const lines = readFileSync(f, "utf8").split(/\r?\n/);
  lines.forEach((line, i) => {
    if (pattern.test(line)) {
      console.log(`${relative(root, f)}:${i + 1}: ${line.trim().slice(0, 150)}`);
      hits++;
    }
  });
}
console.log(`\n${hits} 处匹配`);
