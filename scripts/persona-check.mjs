// 验收「自定义性格」：
//   1. 设置面板里有性格输入框，能看到当前值和字数上限
//   2. 填进去 → 保存 → 设置文件里存下了
//   3. **真发一条消息**，看回复是否真的按这个人设来（这条才是关键：
//      提示词拼得对不对，只有模型的行为能证明）
const PORT = Number(process.env.CDP_PORT || 9222)

const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
if (!page) { console.error('找不到桌宠页面'); process.exit(1) }
const ws = new WebSocket(page.webSocketDebuggerUrl)
await new Promise((r) => { ws.onopen = r })
let id = 0
const pending = new Map()
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id) } }
const ev = (expr) => new Promise((res) => { const i = ++id; pending.set(i, res); ws.send(JSON.stringify({ id: i, method: 'Runtime.evaluate', params: { expression: expr, awaitPromise: true, returnByValue: true } })) })
const invoke = async (cmd, args = {}) => {
  const r = await ev(`(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e).slice(0, 200) } } })()`)
  return r.result.result.value
}
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
let fails = 0
const check = (n, ok, extra = '') => { console.log(`${ok ? '✅' : '❌'} ${n}${extra ? '  ' + extra : ''}`); if (!ok) fails++ }

/**
 * 等元素出现。
 * 设置面板是"**先加载设置、再显示**"的（openSettings 里 loadSettings().then(...)），
 * 所以点完 ⚙️ 之后面板出现的时刻并不确定 —— 固定睡 1.5 秒会偶发地跑在渲染之前，
 * 那样测出来的是"测试脚本太快"，而不是功能有问题（这次就是这么翻车的）。
 */
async function waitFor(selector, timeout = 8000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    const ok = await ev(`!!document.querySelector(${JSON.stringify(selector)})`)
    if (ok.result.result.value) return true
    await wait(200)
  }
  return false
}

// ── 0) 先记住主人原来的性格（跑完原样放回去） ──
//
// 踩过的坑：这个脚本第一版直接在真实设置上写测试人设、最后又清空 ——
// 主人自己填过的性格就这么被抹了；而 settings.json.bak 只保留**一代**，
// 连续两次写入把它盖没，原值再也找不回来。
// 教训已写进 AGENTS.md：验收脚本不许改动主人的真实配置，必须先读、跑完还原。
const originalPersona = (await invoke('get_settings')).v?.persona ?? ''
console.log(
  originalPersona
    ? `（你原来有自定义性格，跑完会原样放回去：${originalPersona.slice(0, 24)}…）`
    : '（你原来用的是内置人设，跑完保持原样）',
)

// ── 1) 界面 ──
await ev(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
await wait(400)
await ev(`(() => { const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
// 等面板真的渲染出来（见 waitFor 的说明）
const appeared = await waitFor('.settings-panel .persona-input')

/**
 * 等面板**加载完**，而不只是"出现"。
 *
 * 踩过的坑：面板是 v-if 切换的，上一次打开时的字段值会留在 DOM 里；
 * 只看"元素在不在"会读到**上一次的旧值**，于是后面"填值→保存"的判断全歪
 * （实测表现为：面板里显示的是上一轮脚本填的值，保存后设置文件没变）。
 * 判据就用"面板里的值 == 磁盘上的值" —— 加载完成的定义本来就是这个。
 */
async function waitForPanelLoaded(timeout = 8000) {
  const deadline = Date.now() + timeout
  let last = ''
  while (Date.now() < deadline) {
    const v = await evalValue(
      `(() => { const b = document.querySelector('.persona-input'); return b ? b.value : null })()`,
    )
    if (v !== null) {
      last = v
      if (v === originalPersona) return true
    }
    await wait(200)
  }
  console.log(`   （面板里的值与磁盘不一致，最后看到：${JSON.stringify(last.slice(0, 20))}）`)
  return false
}
check(
  '0) 设置面板已打开，且加载的是当前设置',
  appeared && (await waitForPanelLoaded()),
  originalPersona ? `与磁盘一致（${originalPersona.length} 字）` : '与磁盘一致（空）',
)

/** 取运行时表达式的值（CDP 响应要自己剥两层，忘了剥就会拿到响应对象当结果用） */
async function evalValue(expr) {
  const r = await ev(expr)
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 200))
  return r.result?.result?.value
}

const ui = await evalValue(`(() => {
  const box = document.querySelector('.persona-input')
  const label = [...document.querySelectorAll('.settings-panel .setting-label')].find((e) => e.textContent.includes('性格'))
  return {
    exists: !!box,
    rows: box?.getAttribute('rows'),
    maxlength: box?.getAttribute('maxlength'),
    placeholder: (box?.getAttribute('placeholder') || '').slice(0, 24),
    counter: label?.querySelector('.setting-value')?.textContent?.trim() || '',
    value: box?.value || '',
  }
})()`)
check('1) 设置面板里有「🎭 性格」多行输入框', ui.exists && ui.rows === '4', `rows=${ui.rows}`)
check('2) 带长度上限与字数显示', ui.maxlength === '1000' && /\d+\/1000/.test(ui.counter), ui.counter)
// 面板里应当显示**当前已保存的**性格：留空 = 内置人设，填过 = 主人自己那段。
// （这条原来是"必须为空" —— 前提是还没人填过；主人一填，它就必然误报。）
check('3) 面板显示的是当前已保存的性格', ui.value === originalPersona, ui.value ? `${ui.value.length} 字` : '（空 = 内置人设）')

/**
 * 轮询等设置文件里的 persona 变成期望值。
 *
 * 原来是固定睡 2 秒 —— 机器一忙就不够，于是测出来的是"脚本等太短"，
 * 而不是功能有问题（和下面 waitFor 那个坑同一类：**别拿固定时间赌异步完成**）。
 */
async function waitForPersona(want, timeout = 8000) {
  const deadline = Date.now() + timeout
  let last = ''
  while (Date.now() < deadline) {
    const v = (await invoke('get_settings')).v?.persona ?? ''
    last = JSON.stringify(v).slice(0, 50)
    if (v === want) return { ok: true, text: last }
    await wait(300)
  }
  return { ok: false, text: last }
}

// ── 2) 填一段人设（挑一个"模型一定会露出马脚"的特征）并保存 ──
const PERSONA = '你是一只高冷的猫娘，说话极简、只用短句，而且每句话的末尾都要加上「喵」。'
await ev(`(() => {
  const box = document.querySelector('.persona-input')
  box.value = ${JSON.stringify(PERSONA)}
  box.dispatchEvent(new Event('input', { bubbles: true }))
  return true
})()`)
await wait(300)
await ev(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
const saved = await waitForPersona(PERSONA)
check('4) 保存后写进了设置', saved.ok, saved.text)

// ── 3) 真发一条消息，看人设有没有生效 ──
console.log('   （这一步会真的调用一次模型，几秒钟）')
const reply = await invoke('chat_with_deepseek', { msg: '你好呀', mode: 'chat' })
const text = reply.ok ? String(reply.v) : ''
check('5) 模型回复成功', reply.ok && text.length > 0, reply.ok ? text.slice(0, 80) : reply.e)
check('6) 回复体现了自定义人设（句尾带「喵」）', text.includes('喵'), text.slice(0, 80))

// ── 4) 清空 → 恢复内置人设，并确认设置文件也清了 ──
await ev(`(() => {
  const btn = [...document.querySelectorAll('.settings-panel .setting-switch')].find((b) => b.textContent.includes('清空'))
  if (btn) btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(300)
await ev(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
const cleared = await waitForPersona('')
check('7) 清空后回到内置人设（且设置文件里为空）', cleared.ok, cleared.text)

// ── 8) 把主人原来的性格原样放回去（这一步绝对不能省） ──
await invoke('save_settings', { persona: originalPersona })
const restored = await invoke('get_settings')
check(
  '8) 跑完把你原来的性格原样放回去（验收不许动主人的配置）',
  (restored.v?.persona ?? '') === originalPersona,
  originalPersona ? `已还原 ${originalPersona.slice(0, 20)}…` : '原本就是内置人设',
)

ws.close()
console.log(fails === 0 ? '\n性格功能验收全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)

