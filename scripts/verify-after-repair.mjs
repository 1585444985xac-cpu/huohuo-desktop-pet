// 修复后的界面核对：三个面板都能开、文字无乱码、都能拖动。
const PORT = Number(process.env.CDP_PORT || 9222)

async function findPage() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json`)
      const p = (await r.json()).find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      if (p) return p
    } catch {}
    await new Promise((r) => setTimeout(r, 500))
  }
  throw new Error('连不上调试端口 ' + PORT)
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
  if (r.result?.exceptionDetails) throw new Error('页面异常: ' + JSON.stringify(r.result.exceptionDetails.exception?.description || r.result.exceptionDetails.text))
  return r.result?.result?.value
}

const rep = await evaluate(`(async () => {
  const out = { ok: true, steps: [] }
  const sleep = ms => new Promise(r => setTimeout(r, ms))
  const log = s => out.steps.push(s)
  const fail = s => { out.ok = false; out.steps.push('❌ ' + s) }
  const hasCJK = s => /[\\u4e00-\\u9fff]/.test(s)
  const mojibake = s => /\\uFFFD|锟|鍜|锛|鏂|鑻|鈥|锠/.test(s)

  localStorage.clear()
  const canvas = document.querySelector('canvas')
  if (!canvas) { fail('没有 canvas'); return out }

  const mk = (t, x, y) => new PointerEvent(t, { bubbles: true, cancelable: true, clientX: x, clientY: y,
    pointerId: 1, pointerType: 'mouse', button: 0, buttons: t === 'pointerup' ? 0 : 1, isPrimary: true })

  /** 拖一个面板，返回位移 */
  async function dragPanel(panel, dx) {
    const h = panel.querySelector('.panel-drag-handle')
    if (!h) return null
    const b0 = panel.getBoundingClientRect()
    const hx = b0.left + 100, hy = b0.top + 12
    h.dispatchEvent(mk('pointerdown', hx, hy))
    for (let i = 1; i <= 10; i++) { h.dispatchEvent(mk('pointermove', hx + dx * i / 10, hy)); await sleep(25) }
    h.dispatchEvent(mk('pointerup', hx + dx, hy))
    await sleep(250)
    return Math.round(panel.getBoundingClientRect().left - b0.left)
  }

  async function openPie() {
    canvas.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 600, clientY: 700 }))
    await sleep(400)
    return document.querySelector('.pie-container')
  }
  async function clickPieCenter() {
    const pie = document.querySelector('.pie-container')
    const gs = pie && pie.querySelectorAll('svg g[style*="cursor"]')
    if (gs && gs.length) gs[gs.length - 1].dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await sleep(900)
  }
  /** 点饼图里的第 i 项（项是 <path style="cursor: pointer">，顺序同 pieMenuItems） */
  async function clickPieItem(label) {
    const pie = document.querySelector('.pie-container')
    if (!pie) return false
    const order = ['聊天', 'QQ', '动作', '花园', '漂浮', 'BGM', '酷狗']
    const i = order.indexOf(label)
    const paths = [...pie.querySelectorAll('path')].filter(p => (p.style.cursor || '') === 'pointer')
    if (i < 0 || !paths[i]) return false
    paths[i].dispatchEvent(new MouseEvent('click', { bubbles: true }))
    await sleep(1000)
    return true
  }

  // ── 1) 设置面板 ──
  await openPie(); await clickPieCenter()
  const panel = document.querySelector('.settings-panel')
  if (!panel) { fail('设置面板没打开'); return out }
  log('✓ 设置面板已打开')

  const rows = [...panel.querySelectorAll('.setting-row')].map(r =>
    (r.querySelector('.setting-label')?.textContent || '').trim().replace(/\\s+/g, ' '))
  out.settingsRows = rows
  for (const r of rows) if (mojibake(r)) fail('小节标题乱码: ' + r)
  if (!rows.some(r => hasCJK(r))) fail('小节标题一个中文都没有')
  else log('✓ ' + rows.length + ' 个小节，标题正常')

  const hints = [...panel.querySelectorAll('.setting-hint, .setting-switch-label, .setting-warn, .setting-test-result')]
    .map(e => e.textContent.trim())
  const badH = hints.filter(mojibake)
  if (badH.length) fail('提示文字乱码: ' + badH.slice(0, 2).join(' | '))
  else log('✓ ' + hints.length + ' 条提示文字无乱码')

  const txt = panel.textContent
  const wants = ['收件邮箱', '文件模式的处理能力', '开机自启动', '自动识别', '测试连接', '不是登录密码']
  const missing = wants.filter(w => !txt.includes(w))
  if (missing.length) fail('缺少文案: ' + missing.join('、'))
  else log('✓ 关键文案齐全')

  // 复位后再拖（不然起点贴着左边界会被夹住，测不出真实位移）
  panel.querySelector('.panel-drag-handle').dispatchEvent(new MouseEvent('dblclick', { bubbles: true }))
  await sleep(400)
  out.settingsAutoLeft = Math.round(panel.getBoundingClientRect().left)
  const dx1 = await dragPanel(panel, -180)
  if (dx1 === null) fail('设置面板没有拖动把手')
  else if (Math.abs(dx1 + 180) > 6) fail('设置面板位移异常: ' + dx1)
  else log('✓ 设置面板可拖动（-' + Math.abs(dx1) + 'px，起点 ' + out.settingsAutoLeft + '）')
  panel.querySelector('.chat-close')?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  await sleep(300)

  // ── 2) 聊天面板 ──
  await openPie()
  const okChat = await clickPieItem('聊天')
  if (!okChat) fail('饼图里没找到「聊天」项')
  const chat = document.querySelector('.chat-panel')
  if (!chat) { fail('聊天面板没打开'); return out }
  const ct = chat.textContent
  if (mojibake(ct)) fail('聊天面板有乱码')
  for (const w of ['跟藿藿聊天', '问答', '文件']) if (!ct.includes(w)) fail('聊天面板缺少: ' + w)
  log('✓ 聊天面板正常')
  const dx2 = await dragPanel(chat, -150)
  if (dx2 === null) fail('聊天面板没有拖动把手')
  else if (Math.abs(dx2 + 150) > 6) fail('聊天面板位移异常: ' + dx2)
  else log('✓ 聊天面板可拖动（-' + Math.abs(dx2) + 'px）')
  out.chatStored = localStorage.getItem('huohuo_chat_pos')

  // ── 3) 记忆面板 ──
  chat.querySelector('.memory-btn')?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
  await sleep(1000)
  const mem = document.querySelector('.memory-panel')
  if (!mem) fail('记忆面板没打开')
  else {
    const mt = mem.textContent
    if (mojibake(mt)) fail('记忆面板有乱码')
    else log('✓ 记忆面板正常: ' + mt.trim().replace(/\\s+/g, ' ').slice(0, 36))
    const dx3 = await dragPanel(mem, -120)
    if (dx3 === null) fail('记忆面板没有拖动把手')
    else if (Math.abs(dx3 + 120) > 6) fail('记忆面板位移异常: ' + dx3)
    else log('✓ 记忆面板可拖动（-' + Math.abs(dx3) + 'px）')
  }
  return out
})()`)

console.log('──────── 结果 ────────')
for (const s of rep.steps) console.log(' ', s)
if (rep.settingsRows) { console.log('\n设置小节:'); for (const r of rep.settingsRows) console.log('   ·', r) }
if (rep.chatStored) console.log('\n聊天面板位置已落盘:', rep.chatStored)
console.log('\n结论:', rep.ok ? '✅ 全部通过' : '❌ 有失败项')
ws.close()
process.exit(rep.ok ? 0 : 1)
