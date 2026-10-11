// 发布侧工具：给一个安装包生成签名过的 latest.json。
//
// 用法：
//   node scripts/make-update-manifest.mjs <安装包路径> "<版本说明>" [私钥路径]
//
// 干三件事：
//   1. 算出安装包的 SHA-256、大小
//   2. 用私钥对规范串签名（私钥路径**必须显式给**，见下）
//   3. 写出 latest.json（与安装包同目录），连同安装包一起上传即可
//
// 规范串必须和 src-tauri/src/update.rs 的 canonical() 完全一致 ——
// 两边只要差一个字符，客户端就会报"签名校验失败"。
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'

const MANIFEST_FORMAT = 1
const [, , installerArg, notesArg, keyArg] = process.argv

if (!installerArg) {
  console.error('用法: node scripts/make-update-manifest.mjs <安装包路径> "<版本说明>" <私钥路径>')
  process.exit(1)
}
const installer = path.resolve(installerArg)
if (!fs.existsSync(installer)) {
  console.error(`找不到安装包: ${installer}`)
  process.exit(1)
}

// 私钥路径**刻意不写死在源码里**（安全审查 S11）：它是发布者机器上的私人路径，
// 写进仓库等于每克隆一份就抄走一份 —— 而 README 里本来就已经把它清掉了。
// 现在必须显式给：命令行第 3 个参数，或者环境变量 PET_UPDATE_KEY。
const keyPath = keyArg || process.env.PET_UPDATE_KEY
if (!keyPath) {
  console.error(
    '没给私钥路径。\n' +
      '源码里刻意不留默认值（那是发布者机器上的私人路径，不该进仓库），请显式指定：\n' +
      '  node scripts/make-update-manifest.mjs <安装包路径> "<版本说明>" <私钥.pem>\n' +
      '  或者先设环境变量 PET_UPDATE_KEY 再跑（少了这个参数就是这个报错，不会去猜一个路径）'
  )
  process.exit(1)
}
if (!fs.existsSync(keyPath)) {
  console.error(`找不到私钥: ${keyPath}\n（私钥不进仓库；丢了就只能换密钥并重新发一版程序）`)
  process.exit(1)
}

// 版本号：用安装包的文件名（Tauri 生成的是 desktop-pet_1.0.0_x64-setup.exe）+ 构建时间。
// version_code 才是客户端判断新旧的依据，字符串只是给人看的。
const stat = fs.statSync(installer)

/**
 * 构建戳必须**来自这次构建**，不是"签名这一刻"。
 *
 * 踩过的坑：原来用 Math.floor(Date.now()/1000)，而签名总在构建之后 ——
 * 于是清单永远比它自己打包的那一版"新"，用户点检查更新会被提示
 * "发现新版本 1.0.0"，装完还是同一版，下次开机又问一遍。
 * 所以这里只认 build.rs 写下的 build-epoch.txt（在 target/<profile>/ 下，
 * 也就是安装包往上两层），读不到就直接报错，绝不用当前时间糊弄过去。
 */
function resolveBuildEpoch() {
  if (process.env.BUILD_EPOCH) return Number(process.env.BUILD_EPOCH)
  // <profile>/bundle/nsis/xxx.exe → <profile>
  const profileDir = path.resolve(path.dirname(installer), '..', '..')
  const stampFile = path.join(profileDir, 'build-epoch.txt')
  if (fs.existsSync(stampFile)) {
    const v = Number(fs.readFileSync(stampFile, 'utf8').trim())
    if (Number.isFinite(v) && v > 0) return v
  }
  console.error(
    `找不到构建戳（${stampFile}）。\n` +
      '这是有意的：清单里的 version_code 必须是这次**构建**的时间戳，\n' +
      '否则会比它自己打包的那一版还新，用户会被反复提示更新到同一个版本。\n' +
      '请先 npm run tauri -- build（build.rs 会写出这个文件），或显式传 BUILD_EPOCH=<秒>。',
  )
  process.exit(1)
}
const buildEpoch = resolveBuildEpoch()
const local = new Date(buildEpoch * 1000).toLocaleString('zh-CN', { hour12: false })
// 版本号优先用显式给的（publish.mjs 会传 UPDATE_VERSION）——
// 从文件名猜只是手工跑这个脚本时的兜底
const pkgVersion =
  (process.env.UPDATE_VERSION || '').trim() ||
  (path.basename(installer).match(/_([\d.]+)_/) || [, '1.0.0'])[1]

const bytes = fs.readFileSync(installer)
const sha256 = crypto.createHash('sha256').update(bytes).digest('hex')

const manifest = {
  version: `${pkgVersion} (${local})`,
  version_code: buildEpoch,
  notes: notesArg || '',
  url: process.env.UPDATE_URL || 'https://请把这里换成安装包的真实下载直链/desktop-pet_setup.exe',
  sha256,
  size: stat.size,
  signature: '',
}

// 规范串：v<格式>|<version_code>|<version>|<url>|<sha256>
const canonical = `v${MANIFEST_FORMAT}|${manifest.version_code}|${manifest.version}|${manifest.url}|${manifest.sha256}`
const privateKey = crypto.createPrivateKey(fs.readFileSync(keyPath))
manifest.signature = crypto.sign(null, Buffer.from(canonical, 'utf8'), privateKey).toString('base64')

const out = path.join(path.dirname(installer), 'latest.json')
fs.writeFileSync(out, JSON.stringify(manifest, null, 2), 'utf8')

console.log('✅ 已生成', out)
console.log('   版本:', manifest.version)
console.log('   version_code:', manifest.version_code)
console.log('   安装包:', path.basename(installer), `${(stat.size / 1024 / 1024).toFixed(2)} MB`)
console.log('   sha256:', sha256.slice(0, 16) + '…')
console.log('   签名:', manifest.signature.slice(0, 24) + '…')
if (manifest.url.includes('请把这里换成')) {
  console.log('\n⚠️ url 还是占位符 —— 用 UPDATE_URL=https://... 重跑一次，或手工改 latest.json 里的 url。')
  console.log('   （改了 url 必须重新签名，因为 url 在签名范围内。）')
}
