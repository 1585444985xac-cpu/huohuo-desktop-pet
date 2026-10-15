// 用 CDP 把桌宠界面真渲染出来验收：
//   1. 饼图 8 格齐全、第 4/5/6 格的标签来自设置
//   2. 第 8 格「说明」能打开说明面板
//   3. 没填网址的那格点了会给提示气泡（而不是静默没反应）
//   4. 设置面板里三行网址快捷键显示的是默认值
//   5. 截一张图存下来，人眼再看一眼
//
// 用法：exe 带 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222 启动后
//       node pet-ui-check.mjs
import fs from 'node:fs'

const PORT = Number(process.env.CDP_PORT || 9222)
const SHOT = process.env.SHOT || 'E:\\1\\_diag\\pie-shot.png'

async function findPage() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json`)
      const list = await r.json()
      const p = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      if (p) return p
    } catch { /* 还没起来 */ }
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error(`连不上调试端口 ${PORT}`)
}

const page = await findPage()
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej })

let seq = 0
const pending = new Map()
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) }
}
function send(method, params) {
  return new Promise((res) => {
    const id = ++seq
    pending.set(id, res)
    ws.send(JSON.stringify({ id, method, params }))
  })
}
async function evaluate(expression) {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails))
  return r.result?.result?.value
}
const wait = (ms) => new Promise((r) => setTimeout(r, ms))

let fails = 0
function check(name, ok, extra = '') {
  console.log(`${ok ? '✅' : '❌'} ${name}${extra ? '  ' + extra : ''}`)
  if (!ok) fails++
}

// ── 1) 打开饼图（在画布上派发一次右键） ──
// 先把主人**真实设置里**的三格名称读出来：这条脚本跑的是他自己的 settings.json，
// 他改过名字（比如第二格叫「网址2」）时，写死"花园/网址/BGM"的断言必然误报。
// 默认值 = 后端 LINK_DEFAULTS（网址1/网址2/网址3）。
const settingsNow = await evaluate(`(async () => {
  try { return await window.__TAURI_INTERNALS__.invoke('get_settings') } catch (e) { return null }
})()`)
const expectLabels = [0, 1, 2].map((i) => ((settingsNow?.links?.[i]?.name || '').trim() || ('网址' + (i + 1))))
// 第一个"没填网址"的格子（用来验"画得更暗""点了给提示"）。都填了就退回中间那格。
const emptySlot = (() => {
  const links = settingsNow?.links || []
  const i = links.findIndex((l) => !l.has_url)
  return i >= 0 ? i : 1
})()

await evaluate(`(() => {
  const c = document.querySelector('canvas')
  const r = c.getBoundingClientRect()
  const ev = new MouseEvent('contextmenu', {
    bubbles: true, cancelable: true,
    clientX: r.width * 0.6, clientY: r.height * 0.4,
  })
  c.dispatchEvent(ev)
  return true
})()`)
await wait(400)

const pie = await evaluate(`(() => {
  const paths = [...document.querySelectorAll('.pie-container svg path')]
  const labels = [...document.querySelectorAll('.pie-container svg tspan[font-weight="600"]')]
    .map((t) => t.textContent.trim())
  const fills = paths.map((p) => p.getAttribute('fill'))
  return { sectors: paths.length, labels, fills }
})()`)
check('饼图渲染出 8 格', pie?.sectors === 8, `实际 ${pie?.sectors}`)
check('第 4/5/6 格标签 = 设置里那三格的名字',
  pie?.labels?.[3] === expectLabels[0] && pie?.labels?.[4] === expectLabels[1] && pie?.labels?.[5] === expectLabels[2],
  JSON.stringify(pie?.labels) + '  期望 ' + JSON.stringify(expectLabels))
check('没配网址的那格画得更暗（视觉上能看出"这里还没填"）',
  pie?.fills?.[3 + emptySlot] !== pie?.fills?.[3 + ((emptySlot + 1) % 3)],
  `第${4 + emptySlot}格=${pie?.fills?.[3 + emptySlot]} 对照格=${pie?.fills?.[3 + ((emptySlot + 1) % 3)]}`)
check('原「漂浮」已不在饼图里', !pie?.labels?.includes('漂浮'))

// ── 2) 点没配网址的那格（空的那格）：应当弹提示气泡 ──
await evaluate(`(() => {
  const paths = [...document.querySelectorAll('.pie-container svg path')]
  paths[${3 + emptySlot}].dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(900)
const bubble = await evaluate(`(() => {
  const b = document.querySelector('.idle-bubble')
  return b ? b.textContent.trim() : ''
})()`)
check('点空格子给了提示（不是静默没反应）', /网址|设置/.test(bubble), JSON.stringify(bubble?.slice(0, 60)))

// ── 3) 再开一次饼图，点第 8 格「说明」 ──
await evaluate(`(() => {
  const c = document.querySelector('canvas')
  const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 }))
  return true
})()`)
await wait(300)
await evaluate(`(() => {
  const paths = [...document.querySelectorAll('.pie-container svg path')]
  paths[7].dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(500)
const help = await evaluate(`(() => {
  const p = document.querySelector('.help-panel')
  if (!p) return null
  return {
    title: p.querySelector('.chat-header span')?.textContent.trim(),
    secs: [...p.querySelectorAll('.help-sec')].map((e) => e.textContent.trim()),
    rows: p.querySelectorAll('.help-row').length,
    scrollH: p.querySelector('.help-body').scrollHeight,
    clientH: p.querySelector('.help-body').clientHeight,
  }
})()`)
check('第 8 格能打开说明面板', !!help, help?.title || '')
check('说明面板分了几段（操作/模式/手机/菜单/其他）', (help?.secs?.length || 0) >= 5, JSON.stringify(help?.secs))
check('说明里提到网址快捷键可在设置里改', JSON.stringify(help?.secs || []).includes('环形菜单'))

// 截图（说明面板开着）
const shot = await send('Page.captureScreenshot', { format: 'png' })
if (shot?.result?.data) {
  fs.writeFileSync(SHOT, Buffer.from(shot.result.data, 'base64'))
  console.log(`🖼  截图: ${SHOT}`)
}

// ── 4) 关掉说明，打开设置面板，看三行网址 ──
await evaluate(`(() => {
  const p = document.querySelector('.help-panel .chat-close')
  if (p) p.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(300)
await evaluate(`(() => {
  const c = document.querySelector('canvas')
  const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 }))
  return true
})()`)
await wait(300)
await evaluate(`(() => {
  const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(1200)
// 面板会停在**上次看的那一页**（settingsTab 在内存里留着），而网址快捷键在「🐾 桌宠」页；
// 上一条脚本要是把面板留在别的页，下面这些行就不存在。先钉一下页签。
await evaluate(`(() => { const t = document.querySelector('.setting-tab[data-tab="pet"]')
  if (t) t.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(500)
const panel = await evaluate(`(() => {
  // 只认"网址快捷键"那三行：.link-row 是通用类（程序位置、邮箱那些行也在用），
  // 拿它当选择器会把 QQ / 酷狗那两行也算进来 —— 断言就永远是"有 5 行"。
  // 网址行的标志是里面有 input.link-name（名称框）。
  // （这段是**页面里**的代码，注释里不许出现反引号：外面那层是模板字符串）
  const rows = [...document.querySelectorAll('.link-row')].filter((r) => r.querySelector('input.link-name'))
  return {
    count: rows.length,
    values: rows.map((r) => {
      const ins = r.querySelectorAll('input')
      return [ins[0]?.value || '', ins[1]?.value || '']
    }),
    inputsPerRow: rows.map((r) => r.querySelectorAll('input').length),
    hasLabel: !!document.querySelector('.settings-panel .setting-label'),
    names: rows.map((r) => r.querySelector('input.link-name')?.value || ''),
  }
})()`)
check('设置面板里有 3 行网址快捷键', panel?.count === 3, JSON.stringify(panel?.values))
// 面板显示的名称要和**磁盘上存着的**一致（AGENTS.md 那条：判据用"面板里的值 == 磁盘上的值"）
check('三行的名称框显示的就是设置里存的值',
  JSON.stringify(panel?.names || []) === JSON.stringify((settingsNow?.links || []).map((l) => l.name || '')),
  JSON.stringify(panel?.names) + ' 期望 ' + JSON.stringify((settingsNow?.links || []).map((l) => l.name || '')))
// 这里**不再断言"默认值是哪几个网址"**：
//   ① 面板显示的是**磁盘上存着的值**，而这条脚本跑的是主人真实的 settings.json ——
//      他要是自己填过网址，断言"默认值"必然误报；
//   ② 原来那条断言写的是 `dmhy.org` / `bgm.tv`，那是作者早期的私人主页默认值
//      （早已改成默认全空），留着它等于把私人站点又抄回仓库（安全审查 S11）。
// "默认三格必须是空的"这句话改在源码层面断言 —— 见 scripts/regression-check.mjs。
// 这里只验结构：每行都得有一个名称框和一个网址框，否则面板就是坏的。
check('每行都有一个名称框和一个网址框',
  Array.isArray(panel?.inputsPerRow) && panel.inputsPerRow.length > 0 && panel.inputsPerRow.every((n) => n === 2),
  JSON.stringify(panel?.inputsPerRow))

const shot2 = await send('Page.captureScreenshot', { format: 'png' })
if (shot2?.result?.data) {
  fs.writeFileSync(SHOT.replace('.png', '-settings.png'), Buffer.from(shot2.result.data, 'base64'))
  console.log(`🖼  截图: ${SHOT.replace('.png', '-settings.png')}`)
}

console.log(fails === 0 ? '\n全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
ws.close()
process.exit(fails === 0 ? 0 : 1)
