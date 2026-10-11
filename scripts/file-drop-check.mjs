// 验收「把文件拖到桌宠身上 → 藿藿知道路径」。
//
// 用法（跟其它 CDP 脚本一样）：
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'
//   .\src-tauri\target\debug\desktop-pet.exe
//   node scripts/file-drop-check.mjs
//
// ⚠️ 能自动化的和不能自动化的，这里如实分开 ——
//
// 能自动化（本脚本跑的就是这些）：
//   1. 后端 `inspect_dropped_paths` 的契约：通过 IPC 真的调一次，
//      看 普通文件 / 文件夹 / 禁区 / 不存在 四种情况分别返回什么；
//   2. 拖动提示的样式必须有 `pointer-events: none`（这不是洁癖：
//      提示若会挡住鼠标，elementFromPoint 就会返回提示自己，
//      "拖到哪里算宠物身上"当场判错）；
//   3. 设置里那两个老的拖放目标（拖快捷方式填「程序位置」）没被新功能改坏。
//
// **不能**自动化：
//   Tauri 的拖放事件来自 Windows 的 OLE 拖放，不是网页的 HTML5 DnD ——
//   CDP 的 Input.dispatchDragEvent 只能伪造后者。所以"拖进去"这个动作要人工验一遍：
//
//   【手工步骤】打开资源管理器，把文件拖到藿藿身上：
//     · 悬在她身上时出现虚线提示「📄 松手就把文件交给藿藿」；悬在设置面板上不该出现
//     · 松手后：聊天面板自动打开、模式变成 📁 文件、输入框里是「请看看这个文件：<完整路径> 」
//     · 拖一个 settings.json 进去：不填输入框，而是在回复区明说「安全拒绝…」
//     · 拖整个文件夹进去：提示语变成「这个文件夹」，路径照填
//
// 本脚本**不改主人的任何配置**：只调只读命令 + 读写 %TEMP% 下的探针文件。

import { mkdirSync, writeFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'

const PORT = Number(process.env.CDP_PORT || 9222)
// 连不上调试端口时给一句人话，而不是抛一个 `TypeError: fetch failed`（这条路径现在就能验）
let list
try {
  list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
} catch (e) {
  console.error(
    `连不上调试端口 ${PORT}（${String(e).slice(0, 80)}）。\n` +
      '这个脚本要对着**正在运行、且带调试端口**的桌宠跑：\n' +
      "  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'\n" +
      '  .\\src-tauri\\target\\debug\\desktop-pet.exe'
  )
  process.exit(1)
}
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
if (!page) {
  console.error(`端口 ${PORT} 上没有可用的页面：exe 是不是没带 --remote-debugging-port 启动？`)
  process.exit(1)
}
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej })
let seq = 0
const pending = new Map()
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) } }
const send = (method, params) => new Promise((res) => { const id = ++seq; pending.set(id, res); ws.send(JSON.stringify({ id, method, params })) })
async function evaluate(expression) {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 300))
  return r.result?.result?.value
}
const invoke = (cmd, args = {}) =>
  evaluate(`(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e).slice(0, 300) } } })()`)
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
let fails = 0
const check = (n, ok, extra = '') => { console.log(`${ok ? '✅' : '❌'} ${n}${extra ? '  ' + extra : ''}`); if (!ok) fails++ }

// ── 0) 在 %TEMP% 下造几个真实的落点（跑完删掉，不碰主人的文件）──
const base = join(tmpdir(), 'huohuo-drop-probe')
try { rmSync(base, { recursive: true, force: true }) } catch {}
mkdirSync(join(base, '一个文件夹'), { recursive: true })
const filePath = join(base, '笔记.txt')
const dirPath = join(base, '一个文件夹')
const secretPath = join(base, 'settings.json')
const gonePath = join(base, '早就删了.txt')
writeFileSync(filePath, '你好，藿藿', 'utf8')
writeFileSync(secretPath, '{"llm_api_key":"假值"}', 'utf8')

console.log('── 1) 后端：拖进来的路径被认成什么 ──')
const res = await invoke('inspect_dropped_paths', {
  // 故意夹一个空白串：它应当被后端丢掉
  paths: ['   ', filePath, dirPath, secretPath, gonePath],
})
const info = Array.isArray(res?.v) ? res.v : []
check('IPC 调得通，返回数组', res?.ok === true && Array.isArray(res?.v), res?.e || `len=${info.length}`)
check('空白路径被丢掉（5 个输入 → 4 个结果）', info.length === 4, `实际 ${info.length}`)

const by = (suffix) => info.find((i) => String(i.path || '').endsWith(suffix))
const f = by('笔记.txt')
const d = by('一个文件夹')
const secret = by('settings.json')
const gone = by('早就删了.txt')
check('普通文件 → file，且带大小', f?.kind === 'file' && f?.size > 0, JSON.stringify(f))
check('文件夹 → dir', d?.kind === 'dir', JSON.stringify(d))
check('settings.json → protected（禁区优先，没被当成普通文件）', secret?.kind === 'protected', JSON.stringify(secret))
check('禁区给出「安全拒绝」的理由，而不是只回一个 kind', String(secret?.note || '').includes('安全拒绝'), String(secret?.note || '').slice(0, 50))
check('不存在的路径 → missing，并说明白', gone?.kind === 'missing' && String(gone?.note || '').length > 0, JSON.stringify(gone))
check('正常文件不带多余的说明', f?.note === '', JSON.stringify(f?.note))
check('路径是给人看的样子（没有 \\\\?\\ 前缀）', !String(f?.path || '').startsWith('\\\\?\\'), String(f?.path || ''))

// ── 2) 提示样式：必须有 pointer-events: none ──
console.log('\n── 2) 拖动提示的样式 ──')
const css = await evaluate(`(() => {
  const out = { found: false, pe: null, z: null, selector: null }
  for (const sheet of Array.from(document.styleSheets)) {
    let rules = []
    try { rules = Array.from(sheet.cssRules || []) } catch { continue }
    for (const r of rules) {
      // 样式是 scoped 的，编译后选择器是 .drop-hint[data-v-xxxx]，所以按包含匹配
      if (r.selectorText && r.selectorText.includes('.drop-hint')) {
        out.found = true
        out.selector = r.selectorText
        out.pe = r.style.pointerEvents
        out.z = r.style.zIndex
      }
    }
  }
  return out
})()`)
check('样式里定义了 .drop-hint', css?.found === true, String(css?.selector || '').slice(0, 60))
check('.drop-hint 是 pointer-events: none（否则会挡住落点判定）', css?.pe === 'none', `pointer-events=${css?.pe}`)
check('.drop-hint 压在聊天面板之上（面板开着也要看得见）', Number(css?.z) > 99999, `z-index=${css?.z}`)

// ── 3) 老的拖放目标没被改坏 ──
console.log('\n── 3) 设置里那两个老的拖放目标还在 ──')
await evaluate(`(() => {
  const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 }))
  return true })()`)
await wait(400)
await evaluate(`(() => {
  const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(1500)
const targets = await evaluate(`(() => ({
  qq: !!document.querySelector('[data-drop-target="qq_path"]'),
  kg: !!document.querySelector('[data-drop-target="kugou_path"]'),
  panel: !!document.querySelector('.settings-panel'),
}))()`)
check('设置面板打开了', targets?.panel === true)
check('「程序位置」的两个拖放输入框仍在', targets?.qq === true && targets?.kg === true, JSON.stringify(targets))

// 收尾：关掉面板 + 删探针
await evaluate(`(() => { const b = document.querySelector('.settings-panel .chat-close'); if (b) b.dispatchEvent(new MouseEvent('click', { bubbles: true })); return true })()`)
try { rmSync(base, { recursive: true, force: true }) } catch {}

ws.close()
console.log('\n（别忘了按脚本开头的【手工步骤】真拖一次 —— 那一步没法自动化）')
console.log(fails === 0 ? '\n自动化的部分全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)
