// 校验 skins.ts 里每个 SVG 皮肤：标签闭合、属性引号配对、viewBox 存在。
// 用法：node scripts/validate-skins.mjs
import { readFileSync } from "node:fs";

const src = readFileSync(new URL("../src/skins.ts", import.meta.url), "utf8");

const SVG_HEAD =
  "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 1600 1000' preserveAspectRatio='xMidYMid slice'>";

// 抓 `const NAME = \`${SVG_HEAD} ... \`;` 形式的皮肤常量
const blocks = [...src.matchAll(/const ([A-Z_]+) = `\$\{SVG_HEAD\}\n([\s\S]*?)`;/g)];
if (blocks.length === 0) {
  console.error("没找到任何 SVG 皮肤常量，skins.ts 格式可能变了");
  process.exit(1);
}

const VOID = new Set(["stop", "use", "circle", "rect", "line", "ellipse", "path", "image"]);

let failed = 0;
for (const [, name, body] of blocks) {
  const svg = SVG_HEAD + "\n" + body;
  const problems = [];

  if (!svg.includes("viewBox")) problems.push("缺少 viewBox");
  if (/<text[\s>]/i.test(svg)) problems.push("含 <text>（项目皮肤约定不含文字）");
  if (/"/.test(svg.replace(/\\"/g, "")) && /=\s*"/.test(svg))
    problems.push("含双引号属性（项目约定用单引号，避免与外层 data URI 冲突）");

  // 标签配对检查：忽略自闭合标签与注释
  const stripped = svg.replace(/<!--[\s\S]*?-->/g, "");
  const stack = [];
  for (const m of stripped.matchAll(/<(\/?)([A-Za-z][\w:-]*)([^>]*?)(\/?)>/g)) {
    const [, closing, tag, , selfClose] = m;
    if (VOID.has(tag) || selfClose === "/") continue;
    if (closing) {
      const top = stack.pop();
      if (top !== tag) {
        problems.push(`标签不匹配：期望 </${top ?? "无"}>，实际 </${tag}>`);
        break;
      }
    } else {
      stack.push(tag);
    }
  }
  if (stack.length) problems.push(`未闭合标签：${stack.join(", ")}`);

  if (problems.length) {
    failed++;
    console.error(`[FAIL] ${name}\n   ${problems.join("\n   ")}`);
  } else {
    console.log(`[ ok ] ${name}  (${svg.length} chars)`);
  }
}

const keys = [...src.matchAll(/key: "([a-z0-9]+)"/g)].map((m) => m[1]);
const dupes = keys.filter((k, i) => keys.indexOf(k) !== i);
if (dupes.length) {
  failed++;
  console.error(`[FAIL] duplicate key: ${[...new Set(dupes)].join(", ")}`);
}

console.log(`\n${blocks.length} SVG skins, ${keys.length} SKINS entries`);
if (failed) {
  console.error(`\n${failed} problem(s)`);
  process.exit(1);
}
console.log("all passed");
