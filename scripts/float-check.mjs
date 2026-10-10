// 验收「设置面板开着的时候，桌宠禁止漂浮」。
//
// 为什么这么测：默认要**闲置 5 分钟**才漂浮，验收里等不起；这条规则的关键又是
// "同样是闲置，只差面板开没开"，所以两段必须用同一个阈值对比。做法是把阈值
// 通过 window.__petFloatIdleSeconds 压到 1 秒（代码里留的口子，见 App.vue），
// 然后看窗口**真的动没动** —— 光读代码推理证明不了这件事。
//
//   1) 面板开着 + 已闲置 → 窗口必须一动不动
//   2) 关掉面板 + 同样闲置 → 窗口必须真的开始飘（证明漂浮本身没被写坏）
//   3) 收尾：停掉漂浮、还原阈值、把桌宠放回原来的位置（不许动主人的桌面布局）
//
// 用法：桌宠带 --remote-debugging-port=9222 启动后 node scripts/float-check.mjs
const PORT = Number(process.env.CDP_PORT || 9222)

const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
if (!page) {
  console.error('找不到桌宠页面（exe 是不是没带调试端口启动？）')
  process.exit(1)
}
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((r) => {
  ws.onopen = r
})
let id = 0
const pending = new Map()
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (pending.has(m.id)) {
    pending.get(m.id)(m)
    pending.delete(m.id)
  }
}
const ev = (expr) =>
  new Promise((res) => {
    const i = ++id
    pending.set(i, res)
    ws.send(
      JSON.stringify({
        id: i,
        method: 'Runtime.evaluate',
        params: { expression: expr, awaitPromise: true, returnByValue: true },
      }),
    )
  })
const evalValue = async (expr) => (await ev(expr)).result?.result?.value
const invoke = async (cmd, args = {}) =>
  evalValue(
    `(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e).slice(0, 300) } } })()`,
  )
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
let fails = 0
const check = (n, ok, extra = '') => {
  console.log(`${ok ? '✅' : '❌'} ${n}${extra ? '  ' + extra : ''}`)
  if (!ok) fails++
}

/** 窗口左上角的物理像素坐标（和 App.vue 里 outerPosition() 取的是同一个东西） */
async function outerPos() {
  return await evalValue(
    `(async () => { const r = await window.__TAURI_INTERNALS__.invoke('plugin:window|outer_position', { label: 'main' }); return { x: r.x, y: r.y } })()`,
  )
}
const moved = (a, b) => Math.abs(a.x - b.x) + Math.abs(a.y - b.y)

const before = await outerPos()
check('0) 能读到桌宠窗口位置（后面的判断都以它为准）', !!before, JSON.stringify(before))

// 打开设置面板
await ev(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
await wait(400)
await ev(`(() => { const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(1200)
const panelOpen = await evalValue(`!!document.querySelector('.settings-panel')`)
check('1) 设置面板已打开', panelOpen)

// 把漂浮的闲置阈值压到 1 秒：等 5 秒的巡检周期跑过一轮，足够触发一次漂浮
await evalValue(`(window.__petFloatIdleSeconds = 1, true)`)
console.log('   （漂浮阈值已临时压到 1 秒；等 9 秒看窗口会不会动）')
await wait(9000)
const p1 = await outerPos()
await wait(2500)
const p2 = await outerPos()
check(
  '2) 面板开着：闲置到点也不漂浮（窗口一动不动）',
  moved(p1, p2) === 0,
  `位移 ${moved(p1, p2)} px`,
)

// 关掉面板，同样的闲置条件 → 应当开始漂浮
await ev(`(() => { const b = document.querySelector('.settings-panel .chat-close')
  if (b) b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(500)
const panelClosed = await evalValue(`!document.querySelector('.settings-panel')`)
check('3) 设置面板已关闭', panelClosed)
console.log('   （面板关掉后同样等 9 秒，这次应该飘起来）')
await wait(9000)
const p3 = await outerPos()
await wait(2500)
const p4 = await outerPos()
check(
  '4) 面板关掉后：同样闲置 → 桌宠真的开始漂浮（说明漂浮功能没被写坏）',
  moved(p3, p4) > 5,
  `位移 ${moved(p3, p4)} px`,
)

// ── 收尾 ──
// pointerdown 会触发 stopFloat()（见 setupMouseInteraction 的 onDown），
// 比点一下更干净：不会计成"摸头"的次数。
await ev(`(() => { const c = document.querySelector('canvas')
  c.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true, button: 0, clientX: 10, clientY: 10 })); return true })()`)
await wait(300)
await evalValue(`(window.__petFloatIdleSeconds = 300, true)`)
await invoke('set_window_pos', { x: before.x, y: before.y })
await wait(500)
const after = await outerPos()
check('5) 收尾：停掉漂浮、阈值还原、桌宠放回原位', moved(before, after) === 0, `回到 ${JSON.stringify(after)}`)

ws.close()
console.log(fails === 0 ? '\n漂浮与面板的验收全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)
