// 通过 WebView2 的远程调试端口驱动桌宠界面，验证设置面板的拖动。
//
// 为什么要有这个：设置面板是浮在透明窗口上的 DOM，我（写代码的那个）没法用鼠标点它。
// 之前所有界面改动都只能靠"读代码 + 推理"，这次借 CDP 真的把事件打进去看结果。
//
// 用法：exe 带 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222 启动后
//       node cdp-drag-test.mjs

const PORT = Number(process.env.CDP_PORT || 9222)

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
  throw new Error(`连不上调试端口 ${PORT}（exe 是不是没带 --remote-debugging-port 启动？）`)
}

const page = await findPage()
console.log('调试目标:', page.title || '(无标题)', '|', page.url)

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
  if (r.result?.exceptionDetails) {
    throw new Error('页面里抛异常: ' + JSON.stringify(r.result.exceptionDetails.exception?.description
      || r.result.exceptionDetails.text))
  }
  return r.result?.result?.value
}

// 把整段交互塞进一个表达式里跑：来回几十次 CDP 调用太慢，也容易踩到状态不同步
const report = await evaluate(`(async () => {
  const out = { steps: [], ok: true }
  const sleep = ms => new Promise(r => setTimeout(r, ms))
  const log = (s) => out.steps.push(s)
  const fail = (s) => { out.ok = false; out.steps.push('❌ ' + s) }

  // 从干净状态开始
  localStorage.removeItem('huohuo_settings_pos')

  // ── 1) 右键宠物 → 环形菜单 → 点中心 ⚙️ 打开设置 ──
  const canvas = document.querySelector('canvas')
  if (!canvas) { fail('找不到 canvas'); return out }
  if (!document.querySelector('.pie-container')) {
    canvas.dispatchEvent(new MouseEvent('contextmenu', {
      bubbles: true, cancelable: true, clientX: 600, clientY: 800,
    }))
    await sleep(400)
  }
  const pie = document.querySelector('.pie-container')
  if (!pie) { fail('环形菜单没弹出'); return out }
  log('✓ 环形菜单已展开')

  const centers = pie.querySelectorAll('svg g[style*="cursor"]')
  if (!centers.length) { fail('找不到环形菜单中心按钮'); return out }
  centers[centers.length - 1].dispatchEvent(new MouseEvent('click', { bubbles: true }))
  await sleep(800)

  const panel = document.querySelector('.settings-panel')
  if (!panel) { fail('设置面板没打开'); return out }
  log('✓ 设置面板已打开')

  // ── 2) 把手与光标 ──
  const header = panel.querySelector('.chat-header')
  if (!header) { fail('找不到标题栏'); return out }
  out.headerClass = header.className
  // 类名是 panel-drag-handle（settings-drag-handle 是很久以前的名字，
  // 这条断言一直没跟着改 —— 无论面板怎么改它都会报「没有拖动把手类」）
  if (!header.classList.contains('panel-drag-handle')) fail('标题栏没有拖动把手类')
  else log('✓ 标题栏是拖动把手: ' + header.className)
  out.headerCursor = getComputedStyle(header).cursor
  if (out.headerCursor !== 'move') fail('光标不是 move，实际 ' + out.headerCursor)
  else log('✓ 光标为 move')

  // ── 3) 拖之前的位置 ──
  const before = panel.getBoundingClientRect()
  out.before = { left: Math.round(before.left), top: Math.round(before.top),
                 w: Math.round(before.width), h: Math.round(before.height) }
  log('拖动前: left=' + out.before.left + ' top=' + out.before.top
      + ' (' + out.before.w + '×' + out.before.h + ')')

  const mk = (type, x, y) => new PointerEvent(type, {
    bubbles: true, cancelable: true, composed: true,
    clientX: x, clientY: y, pointerId: 1, pointerType: 'mouse',
    button: type === 'pointerup' ? 0 : 0, buttons: type === 'pointerup' ? 0 : 1,
    isPrimary: true,
  })

  // ── 4) 先单击一次（不移动）——不该把"跟随桌宠"变成固定坐标 ──
  const hx = before.left + 110, hy = before.top + 12
  header.dispatchEvent(mk('pointerdown', hx, hy))
  header.dispatchEvent(mk('pointerup', hx, hy))
  await sleep(150)
  out.posAfterBareClick = localStorage.getItem('huohuo_settings_pos')
  if (out.posAfterBareClick !== null) fail('单击标题栏就写入了位置（应该只在真的拖动后才写）')
  else log('✓ 单击不落位（仍为跟随桌宠）')

  // ── 5) 真的拖：往左上拖 260×? 像素 ──
  const dx = -Math.min(260, before.left - 5)
  const dy = Math.min(200, window.innerHeight - before.bottom - 10)
  const tx = hx + dx, ty = hy + dy
  header.dispatchEvent(mk('pointerdown', hx, hy))
  for (let i = 1; i <= 12; i++) {
    header.dispatchEvent(mk('pointermove', hx + dx * i / 12, hy + dy * i / 12))
    await sleep(20)
  }
  header.dispatchEvent(mk('pointerup', tx, ty))
  await sleep(250)

  const after = panel.getBoundingClientRect()
  out.after = { left: Math.round(after.left), top: Math.round(after.top) }
  out.delta = { dx: out.after.left - out.before.left, dy: out.after.top - out.before.top }
  log('拖动后: left=' + out.after.left + ' top=' + out.after.top
      + '  位移 (' + out.delta.dx + ', ' + out.delta.dy + ')')

  // 位置该跟着指针走（容 4px 误差）
  if (Math.abs(out.delta.dx - dx) > 4 || Math.abs(out.delta.dy - dy) > 4) {
    fail('面板没跟住指针，期望位移 (' + dx + ', ' + dy + ')')
  } else log('✓ 面板跟住了指针')

  // ── 6) 落盘 ──
  out.stored = localStorage.getItem('huohuo_settings_pos')
  if (!out.stored) fail('拖动后没有把位置写进 localStorage')
  else {
    const p = JSON.parse(out.stored)
    if (Math.abs(p.left - out.after.left) > 4 || Math.abs(p.top - out.after.top) > 4) {
      fail('存下来的位置和实际不符: ' + out.stored)
    } else log('✓ 位置已落盘: ' + out.stored)
  }

  // ── 7) 关闭再打开：应回到拖到的位置，而不是跳回桌宠旁边 ──
  panel.querySelector('.chat-close')?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  await sleep(300)
  if (document.querySelector('.settings-panel')) fail('点关闭没关掉面板')
  else log('✓ 面板已关闭')

  canvas.dispatchEvent(new MouseEvent('contextmenu', {
    bubbles: true, cancelable: true, clientX: 600, clientY: 800,
  }))
  await sleep(300)
  const pie2 = document.querySelector('.pie-container')
  const c2 = pie2 && pie2.querySelectorAll('svg g[style*="cursor"]')
  if (c2 && c2.length) c2[c2.length - 1].dispatchEvent(new MouseEvent('click', { bubbles: true }))
  await sleep(900)
  const panel2 = document.querySelector('.settings-panel')
  if (!panel2) { fail('第二次没打开面板'); return out }
  const r2 = panel2.getBoundingClientRect()
  out.reopened = { left: Math.round(r2.left), top: Math.round(r2.top) }
  if (Math.abs(r2.left - out.after.left) > 4 || Math.abs(r2.top - out.after.top) > 4) {
    fail('重开后跳位了: ' + JSON.stringify(out.reopened) + ' 期望 ' + JSON.stringify(out.after))
  } else log('✓ 重开后仍在拖到的位置')

  // ── 8) 双击标题栏 → 恢复跟随桌宠 ──
  const header2 = panel2.querySelector('.chat-header')
  header2.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
  await sleep(300)
  out.afterReset = localStorage.getItem('huohuo_settings_pos')
  const r3 = panel2.getBoundingClientRect()
  out.resetPos = { left: Math.round(r3.left), top: Math.round(r3.top) }
  if (out.afterReset !== null) fail('双击后 localStorage 没清掉: ' + out.afterReset)
  else log('✓ 双击后已清掉记忆位置')
  if (out.resetPos.left === out.reopened.left && out.resetPos.top === out.reopened.top) {
    fail('双击后位置没变，可能没真的复位')
  } else log('✓ 双击后回到自动位置: left=' + out.resetPos.left + ' top=' + out.resetPos.top)

  // ── 9) 顺带看一眼邮箱设置区块是否真的渲染出来了 ──
  const rows = [...panel2.querySelectorAll('.setting-row')].map(r =>
    (r.querySelector('.setting-label')?.textContent || '').trim().replace(/\\s+/g, ' '))
  out.settingRows = rows

  return out
})()`)

console.log('\n──────── 结果 ────────')
for (const s of report.steps) console.log(' ', s)
if (report.settingRows) {
  console.log('\n设置面板的小节:')
  for (const r of report.settingRows) console.log('   ·', r)
}
console.log('\n结论:', report.ok ? '✅ 全部通过' : '❌ 有失败项')
ws.close()
process.exit(report.ok ? 0 : 1)
