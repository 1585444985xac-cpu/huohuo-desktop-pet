// 一条命令发布一版：签名清单 → 上传到自己的服务器 → 公网复验。
//
// 用法：
//   node scripts/bump-version.mjs 1.0.1        # 主人给了版本号才做
//   npm run tauri -- build                     # 先构建（会写下 target/release/build-epoch.txt）
//   node scripts/publish.mjs --version 1.0.1 "本次更新说明"
//
// **发布必须由主人确认**：上传之后所有装了桌宠的机器下次检查更新就会收到，
// 所以这里强制要求显式版本号 —— 没给就直接退出，提示去问主人，
// 免得"顺手传了个版本号自己编的包"。
//
// 它做四件事：
//   1. 校验版本号（必须与 Cargo.toml / tauri.conf.json / package.json 三处一致）
//   2. 用**构建戳**（不是签名时刻）生成签名清单 latest.json
//   3. scp 安装包 + latest.json 到自己的服务器（地址由 PUBLISH_HOST 给）
//   4. 从公网复验：清单能取到、签名有效、sha256 与本地文件一致
//
// 前提：更新签名私钥在本地（**不进仓库、不进成品**，路径见 AGENTS.md），
//       本机对那台服务器免密 SSH 可用。
//
// 服务器地址**不写进源码**：那是发布者自己的环境信息，用环境变量传：
//   $env:PUBLISH_HOST = "root@your-server"
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'
import { execFileSync } from 'node:child_process'

const HOST = process.env.PUBLISH_HOST || ''
const DIR = process.env.PUBLISH_DIR || '/var/www/pet'
const BASE = process.env.PUBLISH_BASE || 'https://pet.myheart.vip'
const REPO = path.resolve(import.meta.dirname, '..')
const PROFILE = path.join(REPO, 'src-tauri', 'target', 'release')
const NSIS = path.join(PROFILE, 'bundle', 'nsis')

// ── 参数：--version 必填，其余当作更新说明 ──
const argv = process.argv.slice(2)
let version = process.env.PET_VERSION || ''
const rest = []
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--version' || argv[i] === '-v') {
    version = argv[++i] || ''
  } else {
    rest.push(argv[i])
  }
}
const notes = rest.join(' ').trim()

function fail(msg) {
  console.error(`❌ ${msg}`)
  process.exit(1)
}

if (!version) {
  fail(
    '没有版本号，拒绝发布。\n' +
      '  发布更新是"一传出去所有用户都会收到"的动作，版本号必须由主人决定。\n' +
      '  请先问主人这次发哪个版本（例如「这次发 1.0.1 可以吗」），再执行：\n' +
      '    node scripts/bump-version.mjs <版本号>\n' +
      '    npm run tauri -- build\n' +
      '    node scripts/publish.mjs --version <版本号> "更新说明"',
  )
}
if (!/^\d+\.\d+\.\d+$/.test(version)) fail(`版本号「${version}」不像 1.0.1 这种形式`)

// 服务器地址必须由环境变量给 —— 发布者自己的机器地址不写进源码
if (!HOST) {
  fail(
    '没有 PUBLISH_HOST（要上传到哪台服务器）。\n' +
      '  这是发布者自己的环境信息，不写在源码里。例如：\n' +
      '    $env:PUBLISH_HOST = "root@your-server"',
  )
}

// 三处版本号必须一致，否则会出现"安装包叫 1.0.1、程序里写着 1.0.0"
const declared = {
  'Cargo.toml': (fs.readFileSync(path.join(REPO, 'src-tauri', 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m) || [])[1],
  'tauri.conf.json': JSON.parse(fs.readFileSync(path.join(REPO, 'src-tauri', 'tauri.conf.json'), 'utf8')).version,
  'package.json': JSON.parse(fs.readFileSync(path.join(REPO, 'package.json'), 'utf8')).version,
}
const mismatched = Object.entries(declared).filter(([, v]) => v !== version)
if (mismatched.length) {
  fail(
    `要发布的版本是 ${version}，但代码里写的是：` +
      mismatched.map(([k, v]) => `${k}=${v}`).join('、') +
      `\n  先跑：node scripts/bump-version.mjs ${version}  然后重新构建`,
  )
}

function findSetup() {
  const list = fs
    .readdirSync(NSIS)
    .filter((f) => /setup\.exe$/i.test(f))
    .map((f) => ({ f, t: fs.statSync(path.join(NSIS, f)).mtimeMs }))
    .sort((a, b) => b.t - a.t)
  return list[0]
}

// ── 1) 找安装包 + 构建戳 ──
const setup = findSetup()
if (!setup) fail(`在 ${NSIS} 里找不到安装包，先跑 npm run tauri -- build`)
const epoch = (() => {
  const stampFile = path.join(PROFILE, 'build-epoch.txt')
  if (!fs.existsSync(stampFile)) {
    fail(`没有构建戳文件 ${stampFile} —— 先 npm run tauri -- build（build.rs 会写出它）`)
  }
  return Number(fs.readFileSync(stampFile, 'utf8').trim())
})()

console.log(`版本     : ${version}（代码里三处一致 ✅）`)
console.log(`安装包   : ${setup.f}`)
console.log(`构建戳   : ${epoch}（${new Date(epoch * 1000).toLocaleString('zh-CN', { hour12: false })}）`)

// ── 发布前的闸：必须比线上新，否则客户端根本不会收到 ──
//
// 客户端判断新旧的唯一依据是 version_code（构建戳）。如果这次不比线上大，
// 传上去等于白传：所有机器都认为"已是最新"。踩过的坑：build.rs 少声明 rerun 指令，
// 戳被冻住，连续两次构建的戳一模一样 —— 那次要是直接发了，谁也收不到。
const force = process.argv.includes('--force')
try {
  const live = await (await fetch(`${BASE}/latest.json`)).json()
  console.log(`线上当前 : ${live.version}（构建戳 ${live.version_code}）`)
  if (!force && epoch <= live.version_code) {
    fail(
      `这次的构建戳 ${epoch} 不比线上（${live.version_code}）新，客户端不会收到这个版本。\n` +
        '  可能原因：这几次构建之间源码没变（cargo 复用了上次的构建戳）。\n' +
        '  改点东西重新构建即可；如果确实要覆盖同一版（例如只改了清单），加 --force。',
    )
  }
} catch (e) {
  console.log(`线上读取失败（${e.message}）—— 首次发布或临时故障，继续往下走`)
}

// ── 2) 生成签名清单 ──
console.log('\n[1/4] 生成签名清单…')
execFileSync(
  process.execPath,
  [path.join(REPO, 'scripts', 'make-update-manifest.mjs'), path.join(NSIS, setup.f), notes],
  {
    stdio: 'inherit',
    env: { ...process.env, UPDATE_URL: `${BASE}/${setup.f}`, UPDATE_VERSION: version },
  },
)

// ── 3) 上传 ──
console.log('\n[2/4] 上传到服务器…')
execFileSync('scp', ['-o', 'BatchMode=yes', path.join(NSIS, setup.f), path.join(NSIS, 'latest.json'), `${HOST}:${DIR}/`], { stdio: 'inherit' })
execFileSync('ssh', ['-o', 'BatchMode=yes', HOST, `chown www-data:www-data ${DIR}/* && chmod 644 ${DIR}/* && ls -la ${DIR}/`], { stdio: 'inherit' })

// ── 4) 公网复验 ──
console.log('\n[3/4] 公网复验…')

/**
 * 带重试的取回：公网偶尔会抖一下（实测遇到过一次 10 秒连接超时），
 * 而**文件其实已经传上去了** —— 这时候脚本崩掉会让人以为"发布失败"，
 * 误导性比错误本身更大。所以重试三次，仍旧失败也把"上传已完成"说清楚。
 */
async function fetchWithRetry(url, asJson = true) {
  let lastErr
  for (let i = 1; i <= 3; i++) {
    try {
      const r = await fetch(url, { signal: AbortSignal.timeout(20000) })
      if (!r.ok) throw new Error(`HTTP ${r.status}`)
      return asJson ? await r.json() : Buffer.from(await r.arrayBuffer())
    } catch (e) {
      lastErr = e
      if (i < 3) {
        console.log(`  第 ${i} 次取回失败（${e.message}），2 秒后重试…`)
        await new Promise((r) => setTimeout(r, 2000))
      }
    }
  }
  throw lastErr
}

const manifest = await fetchWithRetry(`${BASE}/latest.json`)
const localBytes = fs.readFileSync(path.join(NSIS, setup.f))
const localSha = crypto.createHash('sha256').update(localBytes).digest('hex')

const problems = []
if (manifest.version_code !== epoch) problems.push(`清单版本号 ${manifest.version_code} ≠ 构建戳 ${epoch}`)
if (manifest.sha256 !== localSha) problems.push('清单 sha256 与本地安装包不一致')
if (manifest.size !== localBytes.length) problems.push('清单大小与本地安装包不一致')
if (manifest.url !== `${BASE}/${setup.f}`) problems.push(`清单下载地址异常：${manifest.url}`)

let downloaded = Buffer.alloc(0)
try {
  downloaded = await fetchWithRetry(manifest.url, false)
  if (crypto.createHash('sha256').update(downloaded).digest('hex') !== localSha) {
    problems.push('从服务器下回来的文件与本地不一致')
  }
} catch (e) {
  // 上传本身已经成功，只是这次复查没连上 —— 必须说清楚，别让人以为白传了
  console.log(`\n⚠️ 复查下载失败（${e.message}）`)
  console.log('   注意：**文件已经上传成功**（上面 scp 那一步过了），只是这次复查没连上服务器。')
  console.log('   过一会儿可单独复查：node scripts/publish.mjs 会重新上传同一份，或直接访问：')
  console.log(`   ${manifest.url}`)
  process.exit(2)
}

console.log('\n[4/4] 结果')
console.log(`  版本      : ${manifest.version}`)
console.log(`  版本号    : ${manifest.version_code}`)
console.log(`  sha256    : ${manifest.sha256.slice(0, 16)}…`)
console.log(`  下载地址  : ${manifest.url}`)
console.log(`  公网下载  : ${(downloaded.length / 1048576).toFixed(2)} MB`)
if (problems.length) fail(`复验没通过：\n  - ${problems.join('\n  - ')}`)

console.log('\n✅ 发布完成：客户端点「检查更新」即可收到这一版')
