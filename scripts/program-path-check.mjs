// 验收「程序位置（QQ / 酷狗）+ 拖快捷方式解析」：
//   1. 设置面板里出现两个可拖放的输入框（虚线边框提示 + data-drop-target）
//   2. 落点→目标 的换算正确（elementFromPoint 能命中那个框）
//   3. 后端 resolve_shortcut 真能解析桌面快捷方式 → 真实 .exe 路径
//   4. 保存校验：填错的路径当场报错；有效路径能存能读；最后清理回"自动查找"
const PORT = Number(process.env.CDP_PORT || 9222)
const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
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
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
let fails = 0
const check = (name, ok, extra = '') => { console.log(`${ok ? '✅' : '❌'} ${name}${extra ? '  ' + extra : ''}`); if (!ok) fails++ }

// ── 0) 先记住主人原来填的程序位置 ──
// 这条脚本跑的是**主人真实的 settings.json**：下面第 4 步要写测试值、还要清空，
// 不还原就等于把主人自己填的 QQ 路径抹掉（2026-10-15 真的抹过一次，靠面板上的
// 回显和 WebView2 的 leveldb 里的旧值才查回来）。所以：开头快照，结尾原样写回。
const getSettings = () => evaluate(`window.__TAURI_INTERNALS__.invoke('get_settings')`)
const saveSettings = (args) => evaluate(`(async () => {
  try { await window.__TAURI_INTERNALS__.invoke('save_settings', ${JSON.stringify(args)}); return { ok: true } }
  catch (e) { return { ok: false, err: String(e) } }
})()`)
const before = await getSettings()
const origQq = before?.qq_path || ''
const origKg = before?.kugou_path || ''
console.log(`（跑之前 QQ=${JSON.stringify(origQq)} 酷狗=${JSON.stringify(origKg)}，结束会原样写回）`)

// 打开设置面板
await evaluate(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
await wait(400)
await evaluate(`(() => {
  const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(1500)
// 面板会停在**上次看的那一页**（settingsTab 在内存里留着），而程序位置在「🐾 桌宠」页；
// 上一条脚本要是把面板留在别的页，这里的 QQ / 酷狗 输入框就不存在。先钉一下页签。
await evaluate(`(() => { const t = document.querySelector('.setting-tab[data-tab="pet"]')
  if (t) t.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(500)

// 1) 两个拖放目标 + 虚线样式
const dom = await evaluate(`(() => {
  const qq = document.querySelector('[data-drop-target="qq_path"]')
  const kg = document.querySelector('[data-drop-target="kugou_path"]')
  const info = (el) => el ? {
    value: el.value,
    dashed: getComputedStyle(el).borderTopStyle,
    placeholder: el.placeholder,
    w: Math.round(el.getBoundingClientRect().width),
  } : null
  // 2) 落点换算：取框的中心，看 elementFromPoint 能不能找到它
  //    注意先把这一行滚进可见区域 —— 设置面板内容比可视区高得多，
  //    对**视口外**的点 elementFromPoint 本来就会返回 null（这不是拖放的问题）
  let hit = null
  if (qq) {
    qq.scrollIntoView({ block: 'center' })
    const r = qq.getBoundingClientRect()
    const el = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)
    hit = el?.closest?.('[data-drop-target]')?.getAttribute('data-drop-target') || null
  }
  const lab = [...document.querySelectorAll('.settings-panel .setting-label')].map((e) => e.textContent.trim())
  return { qq: info(qq), kg: info(kg), hit, labels: lab.filter((t) => /程序位置|网址快捷键/.test(t)) }
})()`)
check('设置面板里有 QQ / 酷狗 两个可拖放输入框', !!dom?.qq && !!dom?.kg, JSON.stringify([dom?.qq?.w, dom?.kg?.w]))
check('输入框用虚线边框提示"可以拖进来"', dom?.qq?.dashed === 'dashed', `border-style=${dom?.qq?.dashed}`)
check('落点换算正确（elementFromPoint 命中同一个框）', dom?.hit === 'qq_path', String(dom?.hit))
// 不写"默认必须是空的"：面板显示的是**磁盘上存着的值**，主人自己填过 QQ 路径时
// 那条断言必然误报。改成钉住 AGENTS.md 里那条判据 —— 面板里的值 == 磁盘上的值。
check('面板里显示的就是设置里存的值（没存过 = 空 = 自动查找）',
  (dom?.qq?.value || '') === origQq && (dom?.kg?.value || '') === origKg,
  JSON.stringify([dom?.qq?.value, dom?.kg?.value]) + ' 磁盘 ' + JSON.stringify([origQq, origKg]))
check('面板里有「🖥 程序位置」这一节', JSON.stringify(dom?.labels || []).includes('程序位置'), JSON.stringify(dom?.labels))

// 3) 真的调一次 resolve_shortcut：把桌面上的桌宠快捷方式解析成 .exe
// 桌面路径不写死用户名：从当前用户环境推导，换台机器也能跑同一个脚本
const desktopShortcut = `${process.env.USERPROFILE}\\Desktop\\藿藿桌宠.lnk`
const resolved = await evaluate(`(async () => {
  const p = ${JSON.stringify(desktopShortcut)}
  try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke('resolve_shortcut', { path: p }) } }
  catch (e) { return { ok: false, v: String(e) } }
})()`)
check('resolve_shortcut 能把桌面快捷方式解析成真实路径',
  resolved?.ok && /desktop-pet\.exe$/i.test(resolved.v || ''), JSON.stringify(resolved?.v))
const passthrough = await evaluate(`(async () => {
  try { return await window.__TAURI_INTERNALS__.invoke('resolve_shortcut', { path: 'C:\\\\Windows\\\\System32\\\\notepad.exe' }) }
  catch (e) { return 'ERR:' + String(e) }
})()`)
check('非 .lnk 原样返回（前端可以无脑调用）', /notepad\.exe$/i.test(passthrough || ''), String(passthrough))

// 4) 保存校验：错路径当场报错，好路径能存能读，最后清回自动查找
const badSave = await evaluate(`(async () => {
  try { await window.__TAURI_INTERNALS__.invoke('save_settings', { qqPath: 'C:\\\\no\\\\such\\\\qq.exe' }); return 'NO_ERROR' }
  catch (e) { return String(e) }
})()`)
check('填错的路径在保存时被拦住', /没有文件/.test(badSave), badSave)

const goodSave = await evaluate(`(async () => {
  const exe = ${JSON.stringify(resolved?.v || '')}
  try {
    await window.__TAURI_INTERNALS__.invoke('save_settings', { qqPath: exe })
    const s = await window.__TAURI_INTERNALS__.invoke('get_settings')
    return { ok: true, saved: s.qq_path }
  } catch (e) { return { ok: false, err: String(e) } }
})()`)
check('有效路径能存能读回来', goodSave?.ok && goodSave.saved === resolved?.v, JSON.stringify(goodSave))

const cleared = await evaluate(`(async () => {
  try {
    await window.__TAURI_INTERNALS__.invoke('save_settings', { qqPath: '' })
    const s = await window.__TAURI_INTERNALS__.invoke('get_settings')
    return { ok: true, val: s.qq_path }
  } catch (e) { return { ok: false, err: String(e) } }
})()`)
check('清空后回到"自动查找"', cleared?.ok && cleared.val === '', JSON.stringify(cleared))

// 5) 收尾：把主人原来的值原样写回（包括"他本来就没填"这种情况）
const restored = await saveSettings({ qqPath: origQq, kugouPath: origKg })
const after = await getSettings()
check('收尾：主人的程序位置原样还回去了',
  restored?.ok && (after?.qq_path || '') === origQq && (after?.kugou_path || '') === origKg,
  JSON.stringify({ qq: after?.qq_path, kugou: after?.kugou_path }))

console.log(fails === 0 ? '\n全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
ws.close()
process.exit(fails === 0 ? 0 : 1)
