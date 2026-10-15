// 验收全局快捷键：录制 → 保存 → 真的注册上 → 冲突时如实报错 → 收尾不留副作用
//
// 用法：桌宠带 --remote-debugging-port=9222 启动后 node scripts/hotkey-check.mjs
const PORT = Number(process.env.CDP_PORT || 9222)

const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
if (!page) { console.error('找不到桌宠页面（没带调试端口启动？）'); process.exit(1) }
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
  evaluate(`(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e) } } })()`)
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
let fails = 0
const check = (n, ok, extra = '') => { console.log(`${ok ? '✅' : '❌'} ${n}${extra ? '  ' + extra : ''}`); if (!ok) fails++ }

/** 打开设置面板 */
async function openSettings() {
  await evaluate(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
    c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
  await wait(400)
  await evaluate(`(() => { const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
    g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(1500)
  // 面板会停在**上次看的那一页**（settingsTab 在内存里留着），而快捷键在「🐾 桌宠」页；
  // 上一条脚本要是把面板留在别的页，这里的 .hotkey-input 就不存在。先钉一下页签。
  await evaluate(`(() => { const t = document.querySelector('.setting-tab[data-tab="pet"]')
    if (t) t.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
  await wait(500)
}

const statusOf = async (id) => {
  const s = await invoke('hotkey_status')
  return (s.v?.items || []).find((x) => x.id === id) || {}
}

// ── 0) 先记住主人原来的三个快捷键 ──
// 这条脚本跑的是**主人真实的 settings.json**：下面要录制、保存、清空，
// 不还原就等于把他自己设的组合键抹掉（2026-10-15 真的抹掉过一次：
// 主人把「聊天面板开关」设成了 Ctrl+Alt+H，脚本跑完就没了）。
// 所以：开头快照，结尾原样写回 + apply_hotkeys 让它重新注册。
const before = await invoke('get_settings')
const orig = {
  chat: before.v?.hotkey_chat || '',
  toggle: before.v?.hotkey_toggle || '',
  action: before.v?.hotkey_action || '',
}
console.log(`（跑之前 chat=${JSON.stringify(orig.chat)} toggle=${JSON.stringify(orig.toggle)} `
  + `action=${JSON.stringify(orig.action)}，结束会原样写回并重新注册）`)

await openSettings()

// ── 1) 界面：三个可录制的输入框 + 状态区 ──
const ui = await evaluate(`(() => {
  const rows = [...document.querySelectorAll('.hotkey-row')]
  const label = [...document.querySelectorAll('.settings-panel .setting-label')].find((e) => e.textContent.includes('全局快捷键'))
  return {
    rows: rows.length,
    inputs: document.querySelectorAll('.hotkey-input').length,
    placeholder: rows[0]?.querySelector('input')?.placeholder || '',
    labels: rows.map((r) => r.querySelector('.hotkey-label')?.textContent?.trim()),
    hasStatus: !!document.querySelector('.hotkey-status'),
    hidden: label ? label.parentElement.querySelectorAll('.hotkey-row').length : 0,
  }
})()`)
check('设置面板里有 3 行快捷键', ui.rows === 3 && ui.inputs === 3, JSON.stringify(ui.labels))
check('输入框提示"点这里再按组合键"', /按组合键/.test(ui.placeholder), ui.placeholder)

// ── 2) 录制：在第一个输入框上派发 Ctrl+Alt+H ──
await evaluate(`(() => {
  const input = document.querySelectorAll('.hotkey-input')[0]
  input.focus()
  input.dispatchEvent(new KeyboardEvent('keydown', {
    bubbles: true, cancelable: true, key: 'h', code: 'KeyH', ctrlKey: true, altKey: true,
  }))
  return true
})()`)
await wait(300)
const captured = await evaluate(`document.querySelectorAll('.hotkey-input')[0].value`)
check('按下 Ctrl+Alt+H 被录成 "Ctrl+Alt+H"', captured === 'Ctrl+Alt+H', JSON.stringify(captured))

// 只按修饰键不该写入。
// 先把第二个框清空再按：主人的配置里它本来可能就有值（比如 Ctrl+Alt+Q），
// 不清空的话读到的是"上一次的旧值"，会误报成"写进了半截组合"。
await evaluate(`(() => {
  const row = document.querySelectorAll('.hotkey-row')[1]
  row.querySelector('.link-open').dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(300)
const modOnly = await evaluate(`(() => {
  const input = document.querySelectorAll('.hotkey-input')[1]
  input.focus()
  input.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Control', code: 'ControlLeft', ctrlKey: true }))
  return input.value
})()`)
check('只按修饰键不会写入半截组合', modOnly === '', JSON.stringify(modOnly))

// ── 3) 保存（走界面按钮，验证 diff 布线） ──
await evaluate(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(2200)
const saved = await invoke('get_settings')
check('保存后设置里存下了这个组合键', saved.v?.hotkey_chat === 'Ctrl+Alt+H', JSON.stringify(saved.v?.hotkey_chat))
const st1 = await statusOf('chat')
check('后端真的把它注册上了', st1.ok === true && st1.accel === 'Ctrl+Alt+H', JSON.stringify(st1))

// ── 4) 冲突：Ctrl+Alt+Delete 是系统保留的，必须注册失败并说明原因 ──
//    走界面改（而不是直接 invoke save_settings）：真实路径是"录制 → 保存 → 重新注册 → 刷新状态"
await evaluate(`(() => {
  const input = document.querySelectorAll('.hotkey-input')[0]
  input.focus()
  input.dispatchEvent(new KeyboardEvent('keydown', {
    bubbles: true, cancelable: true, key: 'Delete', code: 'Delete', ctrlKey: true, altKey: true,
  }))
  return true
})()`)
await wait(300)
const captured2 = await evaluate(`document.querySelectorAll('.hotkey-input')[0].value`)
check('录制系统保留组合也能录进去（能不能用由后端说了算）', captured2 === 'Ctrl+Alt+Delete', JSON.stringify(captured2))
await evaluate(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(2600)
const st2 = await statusOf('chat')
check('被系统占用的组合会注册失败', st2.ok === false, JSON.stringify(st2.accel))
check('失败原因是人话（提示被占用/换一个）', /占用|换一个/.test(st2.error || ''), st2.error)

const shown = await evaluate(`(() => {
  const box = document.querySelector('.hotkey-status')
  return box ? box.textContent.replace(/\\s+/g, ' ').trim() : ''
})()`)
check('设置面板里显示出了失败原因', /占用|换一个/.test(shown), shown.slice(0, 90))

// ── 5) 收尾：也走界面清空（直接改文件会让界面里的旧值在下次保存时被写回去） ──
await evaluate(`(() => {
  const row = document.querySelectorAll('.hotkey-row')[0]
  row.querySelector('.link-open').dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return true
})()`)
await wait(300)
check('点 ✕ 能清空输入框', (await evaluate(`document.querySelectorAll('.hotkey-input')[0].value`)) === '')
await evaluate(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(2200)
const st3 = await statusOf('chat')
check('清空后回到"未设置"', st3.accel === '', JSON.stringify(st3))
const final = await invoke('get_settings')
check('设置文件里也不留痕迹', final.v?.hotkey_chat === '', JSON.stringify(final.v?.hotkey_chat))

// ── 6) 收尾：把主人原来的三个快捷键原样写回，并让后端重新注册 ──
// 注意 save_settings 只落盘，**注册要另外调 apply_hotkeys** —— 设置面板上那个
// "保存"按钮就是这么做的（App.vue:1326），只写文件的话要等下次重启才生效。
const back = await evaluate(`(async () => {
  try {
    await window.__TAURI_INTERNALS__.invoke('save_settings', ${JSON.stringify({
      hotkeyChat: orig.chat, hotkeyToggle: orig.toggle, hotkeyAction: orig.action,
    })})
    await window.__TAURI_INTERNALS__.invoke('apply_hotkeys')
    await new Promise((r) => setTimeout(r, 1200))
    const s = await window.__TAURI_INTERNALS__.invoke('get_settings')
    const st = (await window.__TAURI_INTERNALS__.invoke('hotkey_status')).items || []
    return { ok: true, s, st }
  } catch (e) { return { ok: false, err: String(e) } }
})()`)
check('收尾：主人的三个快捷键原样还回去了',
  back?.ok && (back.s?.hotkey_chat || '') === orig.chat
    && (back.s?.hotkey_toggle || '') === orig.toggle
    && (back.s?.hotkey_action || '') === orig.action,
  JSON.stringify([back?.s?.hotkey_chat, back?.s?.hotkey_toggle, back?.s?.hotkey_action]))
// 原来非空的那几个，后端也得真的重新注册上（状态里能查到同一个组合）
const stMap = Object.fromEntries((back?.st || []).map((x) => [x.id, x]))
const okReg = ['chat', 'toggle', 'action'].every((id) => {
  const want = orig[id]
  if (!want) return true          // 主人本来就没设过 → 不要求它注册
  return stMap[id]?.accel === want && stMap[id]?.ok === true
})
check('收尾：原来非空的那几个真的重新注册上了', okReg, JSON.stringify(stMap))

ws.close()
console.log(fails === 0 ? '\n全局快捷键验收全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)
