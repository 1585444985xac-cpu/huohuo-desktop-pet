// 把版本号在三个地方改一致：Cargo.toml / tauri.conf.json / package.json
//
// 为什么需要它：程序的版本号来自 Cargo.toml（面板和日志里显示的就是它），
// 安装包的文件名来自 tauri.conf.json，而 npm 那边还有一份。三处不一致时
// 会出现"发出去的安装包叫 1.0.1、程序里却写着 1.0.0"这种让人怀疑人生的现象。
// publish.mjs 会检查这三处是否一致，不一致直接拒绝发布。
//
// 用法：node scripts/bump-version.mjs 1.0.1
import fs from 'node:fs'
import path from 'node:path'

const v = process.argv[2]
if (!/^\d+\.\d+\.\d+$/.test(v || '')) {
  console.error('用法: node scripts/bump-version.mjs <主>.<次>.<修订>    例如 1.0.1')
  process.exit(1)
}

const REPO = path.resolve(import.meta.dirname, '..')
const targets = [
  { label: 'Cargo.toml', file: path.join(REPO, 'src-tauri', 'Cargo.toml'), re: /^version = "([^"]+)"/m },
  { label: 'tauri.conf.json', file: path.join(REPO, 'src-tauri', 'tauri.conf.json'), re: /"version":\s*"([^"]+)"/ },
  { label: 'package.json', file: path.join(REPO, 'package.json'), re: /"version":\s*"([^"]+)"/ },
]

let changed = 0
for (const t of targets) {
  const src = fs.readFileSync(t.file, 'utf8')
  const m = src.match(t.re)
  if (!m) {
    console.error(`❌ ${t.label} 里没找到版本号，先看看文件是不是被改坏了`)
    process.exit(1)
  }
  if (m[1] === v) {
    console.log(`  ${t.label.padEnd(16)} 已经是 ${v}`)
    continue
  }
  fs.writeFileSync(t.file, src.replace(t.re, m[0].replace(m[1], v)))
  console.log(`  ${t.label.padEnd(16)} ${m[1]} → ${v}`)
  changed++
}

console.log(
  changed
    ? `\n✅ 版本号已统一为 ${v}，接着构建：npm run tauri -- build`
    : `\n（三处本来就都是 ${v}，没有改动）`,
)
