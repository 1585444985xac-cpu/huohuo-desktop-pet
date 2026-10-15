// 端到端验证更新链路 —— 但**不真装软件**（装软件会改动系统，不该由测试做）。
//
// 本地起一个 http 服务（故意限速，好让进度条真能观察到），放三份清单，驱动桌宠（CDP）：
//   A 正常清单      → 检查到新版本 → 弹出更新对话框 → 下载进度条走动 → sha256 一致
//   B 签名不对      → 检查阶段就被拒（"签名校验失败"）—— 防"假更新"的那道门
//   C 哈希不对      → 检查能过（签名是真的），下载后被 sha256 拦下（"对不上"）
//   D 界面          → 弹窗内容、稍后/跳过、以及"立即更新"的下载与失败提示
//
// 需要一份发布侧私钥来签测试清单：从环境变量 PET_UPDATE_KEY 取。
// **源码里刻意不留默认路径**（安全审查 S11）：那是发布者机器上的私人路径，
// 写进仓库等于每克隆一份就抄走一份。
// 用法：$env:PET_UPDATE_KEY='<私钥.pem>'; node scripts/update-e2e.mjs
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import crypto from 'node:crypto'

const PORT = Number(process.env.CDP_PORT || 9222)
const KEY = process.env.PET_UPDATE_KEY
const MANIFEST_FORMAT = 1

if (!KEY) {
  console.log('跳过：没设 PET_UPDATE_KEY（端到端验证需要一份私钥来签测试清单；源码里刻意不留默认路径）')
  process.exit(0)
}
if (!fs.existsSync(KEY)) {
  console.log(`跳过：没有私钥 ${KEY}（端到端验证需要它来签测试清单）`)
  process.exit(0)
}

// ── 1) 造假安装包（3 MB）＋ 三份清单 ──
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'pet-update-e2e-'))
const payload = crypto.randomBytes(3 * 1024 * 1024)
fs.writeFileSync(path.join(work, 'payload.bin'), payload)
const realSha = crypto.createHash('sha256').update(payload).digest('hex')
const privateKey = crypto.createPrivateKey(fs.readFileSync(KEY))

function sign(m) {
  const canonical = `v${MANIFEST_FORMAT}|${m.version_code}|${m.version}|${m.url}|${m.sha256}`
  return crypto.sign(null, Buffer.from(canonical, 'utf8'), privateKey).toString('base64')
}
function manifest(overrides = {}) {
  const m = {
    version: '9.9.9 (测试清单)',
    version_code: Math.floor(Date.now() / 1000) + 3600,   // 未来时间 → 一定"有新版"
    notes: '这是端到端测试用的假清单，不会真的安装。\n第二行用来验证多行说明的排版。',
    url: `http://127.0.0.1:__PORT__/payload.bin`,
    sha256: realSha,
    size: payload.length,
    signature: '',
    ...overrides,
  }
  m.url = m.url.replace('__PORT__', String(server.address().port))
  m.signature = sign(m)
  return m
}

// 服务：/payload.bin 限速发送（~1.2 秒），好让进度条真被观察到
const routes = new Map()
const server = http.createServer(async (req, res) => {
  const body = routes.get(req.url)
  if (!body) { res.writeHead(404); res.end('not found'); return }
  if (req.url === '/payload.bin') {
    res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': body.length })
    const CHUNK = 128 * 1024
    for (let off = 0; off < body.length; off += CHUNK) {
      res.write(body.subarray(off, off + CHUNK))
      await new Promise((r) => setTimeout(r, 50))
    }
    res.end()
    return
  }
  res.writeHead(200, { 'Content-Type': 'application/json' })
  res.end(body)
})
await new Promise((r) => server.listen(0, '127.0.0.1', r))

const good = manifest()
const otherKey = crypto.generateKeyPairSync('ed25519').privateKey
const badSig = manifest()
{
  const canonical = `v${MANIFEST_FORMAT}|${badSig.version_code}|${badSig.version}|${badSig.url}|${badSig.sha256}`
  badSig.signature = crypto.sign(null, Buffer.from(canonical, 'utf8'), otherKey).toString('base64')
}
const badHash = manifest({ sha256: 'c'.repeat(64) })
const noSig = { ...good, signature: '' }

routes.set('/payload.bin', payload)
routes.set('/latest.json', Buffer.from(JSON.stringify(good, null, 2)))
routes.set('/latest-bad-sig.json', Buffer.from(JSON.stringify(badSig, null, 2)))
routes.set('/latest-bad-hash.json', Buffer.from(JSON.stringify(badHash, null, 2)))
routes.set('/latest-nosig.json', Buffer.from(JSON.stringify(noSig, null, 2)))
const base = `http://127.0.0.1:${server.address().port}`

// ── 2) 连 CDP ──
async function findPage() {
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
      const p = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      if (p) return p
    } catch { /* 等 */ }
    await new Promise((r) => setTimeout(r, 400))
  }
  throw new Error('连不上桌宠调试端口')
}
const page = await findPage()
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej })
let seq = 0
const pending = new Map()
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) } }
const send = (method, params) => new Promise((res) => { const id = ++seq; pending.set(id, res); ws.send(JSON.stringify({ id, method, params })) })
async function evaluate(expression) {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails))
  return r.result?.result?.value
}
const invoke = (cmd, args = {}) =>
  evaluate(`(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e) } } })()`)
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
const openSettingsPanel = async () => {
  await evaluate(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
    c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
  await wait(400)
  await evaluate(`(() => {
    const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
    g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(1500)
  // 设置面板按用途分了类，「版本更新」在「⚙️ 系统」那一页 —— 先切过去。
  // v-if 收起来的行在 DOM 里不存在，后面找 `.setting-label` 会直接找不到。
  await evaluate(`(() => { const b = document.querySelector('.setting-tab[data-tab="system"]')
    if (b) b.dispatchEvent(new MouseEvent('click', { bubbles: true })); return true })()`)
  await wait(300)
}
/** 点设置面板里那个 ⟳ */
const clickCheckButton = () => evaluate(`(() => {
  const label = [...document.querySelectorAll('.settings-panel .setting-label')].find((e) => e.textContent.includes('版本更新'))
  label.parentElement.querySelector('.link-open').dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true })()`)
const readDialog = () => evaluate(`(() => {
  const d = document.querySelector('.update-panel')
  if (!d) return null
  return {
    title: d.querySelector('.chat-header span')?.textContent?.trim() || '',
    oldV: d.querySelector('.upd-old')?.textContent?.trim() || '',
    newV: d.querySelector('.upd-new')?.textContent?.trim() || '',
    meta: d.querySelector('.upd-meta')?.textContent?.replace(/\\s+/g, ' ').trim() || '',
    notes: d.querySelector('.update-notes')?.textContent?.trim() || '',
    buttons: [...d.querySelectorAll('.upd-actions button')].map((b) => b.textContent.trim()),
    barWidth: d.querySelector('.upd-bar-fill')?.style.width || '',
    progressText: d.querySelector('.upd-progress-text')?.textContent?.replace(/\\s+/g, ' ').trim() || '',
    result: d.querySelector('.setting-test-result')?.textContent?.trim() || '',
    bad: !!d.querySelector('.setting-test-result.bad'),
  }
})()`)

let fails = 0
const check = (name, ok, extra = '') => { console.log(`${ok ? '✅' : '❌'} ${name}${extra ? '  ' + extra : ''}`); if (!ok) fails++ }

try {
  const status = await invoke('update_status')
  check('update_status 报出当前版本与构建戳', status.ok && status.v.build_epoch > 0,
    `${status.v?.version} @ ${new Date(status.v?.build_epoch * 1000).toLocaleString('zh-CN', { hour12: false })}`)

  // ── 场景 A：后端链路（正常清单） ──
  await invoke('save_settings', { updateUrl: `${base}/latest.json` })
  const c1 = await invoke('update_check')
  check('A1 检查到新版本', c1.ok && c1.v.has_update === true, JSON.stringify(c1.v?.message || c1.e))
  const d1 = await invoke('update_download')
  const dlPath = d1.v?.path
  const okSize = d1.ok && fs.existsSync(dlPath) && fs.statSync(dlPath).size === payload.length
  check('A2 下载落盘且大小一致', okSize, `${d1.v?.size} 字节`)
  const gotSha = okSize ? crypto.createHash('sha256').update(fs.readFileSync(dlPath)).digest('hex') : ''
  check('A3 下载内容 sha256 与清单一致', gotSha === realSha, gotSha.slice(0, 16) + '…')

  // ── 场景 B：假的更新 ──
  await invoke('save_settings', { updateUrl: `${base}/latest-bad-sig.json` })
  const c2 = await invoke('update_check')
  check('B1 别人签的清单被拒（检查阶段）', c2.ok === false && /签名/.test(c2.e || ''), String(c2.e).slice(0, 70))
  await invoke('save_settings', { updateUrl: `${base}/latest-nosig.json` })
  const c3 = await invoke('update_check')
  check('B2 没签名的清单被拒', c3.ok === false && /签名/.test(c3.e || ''), String(c3.e).slice(0, 70))

  // ── 场景 C：哈希不对 ──
  await invoke('save_settings', { updateUrl: `${base}/latest-bad-hash.json` })
  const c4 = await invoke('update_check')
  check('C1 签名有效但哈希不符：检查阶段放行', c4.ok && c4.v.has_update === true)
  const d4 = await invoke('update_download')
  check('C2 下载后被 sha256 拦下', d4.ok === false && /对不上/.test(d4.e || ''), String(d4.e).slice(0, 80))

  // ── 场景 D：界面（弹窗 + 进度条 + 按钮） ──
  await invoke('save_settings', { updateUrl: `${base}/latest.json` })
  await openSettingsPanel()

  // 更新源是内置的：面板上不该再有地址输入框（曾经有一个，已经去掉）
  const updRow = await evaluate(`(() => {
    const label = [...document.querySelectorAll('.settings-panel .setting-label')].find((e) => e.textContent.includes('版本更新'))
    if (!label) return null
    const box = label.parentElement
    return {
      inputs: box.querySelectorAll('input').length,
      source: box.querySelector('.update-source')?.textContent?.replace(/\\s+/g, ' ').trim() || '',
      hasButton: !!box.querySelector('.link-open'),
    }
  })()`)
  check('D0 版本更新一节没有地址输入框，只读显示更新源',
    !!updRow?.hasButton && updRow.inputs === 0 && /更新源：/.test(updRow.source || ''),
    `输入框 ${updRow?.inputs} 个 · ${updRow?.source}`)
  await clickCheckButton()
  await wait(2500)
  const dlg = await readDialog()
  check('D1 检查后**自动弹出**更新对话框', !!dlg, dlg?.title || '(没有弹窗)')
  check('D2 显示 当前版本 → 新版本', /→/.test(`${dlg?.oldV}${dlg?.newV}`) || (!!dlg?.oldV && !!dlg?.newV),
    `${dlg?.oldV} → ${dlg?.newV}`)
  check('D3 显示安装包大小与安装方式', /MB/.test(dlg?.meta || ''), dlg?.meta)
  check('D4 更新说明多行显示', (dlg?.notes || '').includes('第二行'), (dlg?.notes || '').slice(0, 30))
  check('D5 三个按钮：立即更新 / 跳过这个版本 / 稍后',
    JSON.stringify(dlg?.buttons).includes('立即更新') && JSON.stringify(dlg?.buttons).includes('跳过') && JSON.stringify(dlg?.buttons).includes('稍后'),
    JSON.stringify(dlg?.buttons))

  // 点「立即更新」→ 观察进度条真的在走
  await evaluate(`(() => { const d = document.querySelector('.update-panel')
    ;[...d.querySelectorAll('.upd-actions button')].find((b) => b.textContent.includes('立即更新')).dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(600)
  const mid = await readDialog()
  check('D6 下载中有进度条和百分比', /%/.test(mid?.progressText || ''), `${mid?.barWidth} | ${mid?.progressText}`)
  await wait(4000)
  const done = await readDialog()
  // 假安装包不是合法 exe，Windows 打不开 → 界面必须把原因说出来（而不是静默）
  check('D7 安装阶段失败有明确提示（假安装包本来就打不开）',
    /❌/.test(done?.result || '') && done?.bad === true, (done?.result || '').slice(0, 80))

  // 「稍后」关掉弹窗
  await evaluate(`(() => { const d = document.querySelector('.update-panel')
    ;[...d.querySelectorAll('.upd-actions button')].find((b) => b.textContent.includes('稍后')).dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(400)
  check('D8「稍后」能关掉弹窗', (await readDialog()) === null)

  // 「跳过这个版本」：手动再检查仍会重新弹（跳过只压制自动检查）
  await clickCheckButton()
  await wait(2500)
  await evaluate(`(() => { const d = document.querySelector('.update-panel')
    ;[...d.querySelectorAll('.upd-actions button')].find((b) => b.textContent.includes('跳过')).dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(400)
  const skipped = await evaluate(`(() => ({ dialog: !!document.querySelector('.update-panel'), mark: localStorage.getItem('huohuo_skip_update_code') }))()`)
  check('D9「跳过这个版本」记住并关窗', !skipped.dialog && Number(skipped.mark) > 0, `mark=${skipped.mark}`)
  await clickCheckButton()
  await wait(2500)
  check('D10 手动检查不受"跳过"影响（仍会弹）', !!(await readDialog()))

  // ── 场景 F：自动检查开关 ──
  // 回归：启动时的自动检查必须读**后端**的设置。曾用前端默认值 → 用户关掉了也照样检查。
  const f1 = await invoke('update_status')
  check('F1 默认开启自动检查，且状态来自后端', f1.ok && f1.v.auto_check === true, `auto_check=${f1.v?.auto_check}`)
  await invoke('save_settings', { updateAutoCheck: false })
  const f2 = await invoke('update_status')
  check('F2 关掉后后端如实报 false（启动逻辑据此跳过检查）', f2.ok && f2.v.auto_check === false)
  await invoke('save_settings', { updateAutoCheck: true })
  const f3 = await invoke('update_status')
  check('F3 能恢复为开', f3.ok && f3.v.auto_check === true)

  // ── 收尾：清掉测试地址与跳过标记，别留在用户环境里 ──
  await invoke('save_settings', { updateUrl: '' })
  await evaluate(`(() => { localStorage.removeItem('huohuo_skip_update_code'); return true })()`)
  const st = await invoke('update_status')
  check('D11 验收不留副作用（设置里的地址已清空，回到内置默认源）',
    st.ok && st.v?.update_url === 'https://pet.myheart.vip/latest.json', `url='${st.v?.update_url}'`)

  // 回到默认源之后，面板上显示的也该是这台服务器（而不是刚才的测试地址）
  await evaluate(`(() => { const b = document.querySelector('.settings-panel .link-open'); b && b.dispatchEvent(new MouseEvent('click', { bubbles: true })); return true })()`)
  await wait(1200)
  const src = await evaluate(`(() => {
    const label = [...document.querySelectorAll('.settings-panel .setting-label')].find((e) => e.textContent.includes('版本更新'))
    return label?.parentElement.querySelector('.update-source')?.textContent?.replace(/\\s+/g, ' ').trim() || ''
  })()`)
  check('D12 面板显示的更新源就是内置的那台服务器', /pet\.myheart\.vip/.test(src), src)
} finally {
  ws.close()
  server.close()
  try { fs.rmSync(work, { recursive: true, force: true }) } catch { /* 忽略 */ }
}

console.log(fails === 0 ? '\n全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)
