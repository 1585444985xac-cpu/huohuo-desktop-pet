// 验收「🤖 模型 API（可换服务商）」：
//
//   1. 设置面板里有这一节：服务商下拉（含多个预设）/ 地址 / 密钥 / 模型 / 代理开关
//   2. 面板上改（不是直接 invoke）→ 保存 → 真的写进设置文件
//   3. **最关键的一条**：把地址指向本地 mock 服务商后，
//      聊天 / 问答 / 文件 **三个模式**发出的请求都打到那个地址、都用那个模型名，
//      并且带着正确的 Bearer 头 —— 这才叫"换了 API 真的生效"。
//   4. 「测试连接」按钮当场给出结论
//   5. 跑完把主人原来的 llm_* 配置**原样放回去**（AGENTS.md：验收不许动主人的真实配置）
//
// 本地 mock 而不是打真接口：不需要任何真密钥、不受网络影响，而且能断言
// "请求里到底写了什么"（真接口只能看到回复，看不到请求形状）。
//
// 用法：桌宠带 --remote-debugging-port=9222 启动后 node scripts/llm-api-check.mjs
import http from 'node:http'

const PORT = Number(process.env.CDP_PORT || 9222)
const MOCK_REPLY = '【mock 服务商】收到'
const MOCK_MODEL = 'mock-model-1'
const MOCK_KEY = 'sk-mock-123'

// ── mock 一个 OpenAI 兼容服务商 ──
const received = []
const mock = http.createServer((req, res) => {
  let body = ''
  req.on('data', (c) => (body += c))
  req.on('end', () => {
    let parsed = {}
    try {
      parsed = JSON.parse(body || '{}')
    } catch {
      /* 记下原文即可 */
    }
    received.push({ path: req.url, auth: req.headers.authorization || '', body: parsed })
    const payload = JSON.stringify({
      id: 'mock-1',
      object: 'chat.completion',
      model: MOCK_MODEL,
      choices: [
        { index: 0, message: { role: 'assistant', content: MOCK_REPLY }, finish_reason: 'stop' },
      ],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    })
    res.writeHead(200, { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(payload) })
    res.end(payload)
  })
})
await new Promise((r) => mock.listen(0, '127.0.0.1', r))
const mockPort = mock.address().port
const mockBase = `http://127.0.0.1:${mockPort}/v1`

// ── 连桌宠的调试端口 ──
let list
try {
  list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
} catch (e) {
  console.error(`连不上调试端口 ${PORT}：exe 是不是没带 --remote-debugging-port 启动？`)
  mock.close()
  process.exit(1)
}
const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
if (!page) {
  console.error('找不到桌宠页面')
  mock.close()
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
/** CDP 响应要自己剥两层，忘了剥就会把响应对象当成结果用 */
async function evalValue(expr) {
  const r = await ev(expr)
  if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 300))
  return r.result?.result?.value
}
const invoke = async (cmd, args = {}) =>
  evalValue(
    `(async () => { try { return { ok: true, v: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)}) } } catch (e) { return { ok: false, e: String(e).slice(0, 300) } } })()`,
  )
const wait = (ms) => new Promise((r) => setTimeout(r, ms))
async function waitFor(selector, timeout = 8000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    if (await evalValue(`!!document.querySelector(${JSON.stringify(selector)})`)) return true
    await wait(200)
  }
  return false
}
let fails = 0
const check = (n, ok, extra = '') => {
  console.log(`${ok ? '✅' : '❌'} ${n}${extra ? '  ' + extra : ''}`)
  if (!ok) fails++
}

// ── 0) 记住主人原来的配置（跑完原样放回去） ──
const before = (await invoke('get_settings')).v || {}
const original = {
  llmProvider: before.llm_provider ?? '',
  llmBaseUrl: before.llm_base_url ?? '',
  llmModel: before.llm_model ?? '',
  llmApiKey: before.llm_api_key ?? '',
  llmUseProxy: before.llm_use_proxy ?? false,
}
console.log(
  `（原配置：服务商=${original.llmProvider || '空'} 地址=${original.llmBaseUrl || '空'} ` +
    `模型=${original.llmModel || '空'} 密钥=${original.llmApiKey ? '有' : '空'} —— 跑完会原样放回去）`,
)

// ── 1) 面板 ──
await ev(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
await wait(400)
await ev(`(() => { const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
const appeared = await waitFor('.settings-panel .llm-provider')

/**
 * 等面板**加载完**，而不只是"出现"：面板是 v-if 切换的，
 * 上一次打开时的值会留在 DOM 里，只看"元素在不在"会读到旧值，
 * 后面所有"填值→保存"的判断就全歪了。判据 = 下拉的值与磁盘一致。
 */
async function waitForPanelLoaded(timeout = 8000) {
  const deadline = Date.now() + timeout
  let last = ''
  while (Date.now() < deadline) {
    const v = await evalValue(
      `(() => { const s = document.querySelector('.settings-panel .llm-provider'); return s ? s.value : null })()`,
    )
    if (v !== null) {
      last = v
      if (v === (before.llm_provider || 'deepseek')) return true
    }
    await wait(200)
  }
  console.log(`   （下拉里的值与磁盘不一致，最后看到：${JSON.stringify(last)}）`)
  return false
}
check(
  '0) 设置面板已打开，且加载的是当前设置',
  appeared && (await waitForPanelLoaded()),
  '等待服务商下拉出现并与磁盘一致',
)

const ui = await evalValue(`(() => {
  const sel = document.querySelector('.settings-panel .llm-provider')
  const rows = [...document.querySelectorAll('.settings-panel .setting-label')].map((e) => e.textContent.trim())
  return {
    hasSection: rows.some((t) => t.includes('模型 API')),
    providers: [...(sel?.options || [])].map((o) => o.value),
    labels: [...(sel?.options || [])].map((o) => o.textContent.trim()),
    hasUrl: !!document.querySelector('.settings-panel .llm-base-url'),
    hasModel: !!document.querySelector('.settings-panel .llm-model'),
    hasKey: !!document.querySelector('.settings-panel .llm-key'),
    endpointShown: (document.querySelector('.settings-panel .llm-endpoint')?.textContent || '').trim(),
    modelChoices: [...document.querySelectorAll('#llm-model-choices option')].map((o) => o.value),
  }
})()`)
check('1) 有「🤖 模型 API」这一节', ui.hasSection)
check(
  '2) 服务商下拉里有多个常见服务商（不再只有 DeepSeek）',
  ui.providers.length >= 8,
  `${ui.providers.length} 个：${ui.labels.slice(0, 4).join(' / ')} …`,
)
check('3) 密钥与模型输入框都在', ui.hasModel && ui.hasKey, `模型=${ui.hasModel} 密钥=${ui.hasKey}`)
// 预设自带默认地址，所以输入框平时是收起来的（只在选「自定义」或已存了自定义地址时露出）。
// 反过来：如果磁盘上已经存着一条自定义地址，输入框**必须**露出来 —— 否则就是个看不见的配置。
if (before.llm_provider === 'custom' || (before.llm_base_url || '').trim()) {
  check('3b) 已存了自定义地址 → 地址输入框露出来（配置不能藏起来）', ui.hasUrl, `输入框=${ui.hasUrl}`)
} else {
  check(
    '3b) 预设下接口地址收起来，只显示"正在用哪条默认地址"',
    !ui.hasUrl && ui.endpointShown.includes('.'),
    `地址输入框=${ui.hasUrl} 显示=${ui.endpointShown || '(空)'}`,
  )
}

// ── 2) 在面板上改成"自定义 + 本地 mock 服务商"并保存 ──
await ev(`(() => {
  const sel = document.querySelector('.settings-panel .llm-provider')
  sel.value = 'custom'
  sel.dispatchEvent(new Event('change', { bubbles: true }))
  return true
})()`)
await wait(300)
const afterSwitch = await evalValue(`(() => ({
  hasUrl: !!document.querySelector('.settings-panel .llm-base-url'),
  urlPlaceholder: document.querySelector('.settings-panel .llm-base-url')?.placeholder || '',
  modelPlaceholder: document.querySelector('.settings-panel .llm-model')?.placeholder || '',
}))()`)
check(
  '4) 切到「自定义」后接口地址输入框**露出来**（这是主人要的那个交互）',
  afterSwitch.hasUrl && afterSwitch.urlPlaceholder.includes('自己填'),
  `输入框=${afterSwitch.hasUrl} 占位=${afterSwitch.urlPlaceholder}`,
)
check(
  '4b) 模型的占位也变成"自己填"',
  afterSwitch.modelPlaceholder.includes('自己填'),
  afterSwitch.modelPlaceholder,
)

await ev(`(() => {
  const set = (sel, val) => {
    const el = document.querySelector(sel)
    el.value = val
    el.dispatchEvent(new Event('input', { bubbles: true }))
  }
  set('.settings-panel .llm-base-url', ${JSON.stringify(mockBase)})
  set('.settings-panel .llm-model', ${JSON.stringify(MOCK_MODEL)})
  set('.settings-panel .llm-key', ${JSON.stringify(MOCK_KEY)})
  return true
})()`)
await wait(300)
await ev(`(() => { const b = document.querySelector('.settings-panel .setting-save')
  b.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(2000)

const saved = (await invoke('get_settings')).v || {}
check(
  '5) 保存后设置里就是新服务商与新模型',
  saved.llm_provider === 'custom' && saved.llm_model === MOCK_MODEL && saved.llm_endpoint === `${mockBase}/chat/completions`,
  `provider=${saved.llm_provider} endpoint=${saved.llm_endpoint}`,
)

// ── 3) 测试连接按钮 ──
const tested = await invoke('test_llm_connection')
check(
  '6) 「测试连接」当场报告成功（并带回 mock 的回复）',
  tested.ok && String(tested.v).includes('连接正常') && String(tested.v).includes(MOCK_REPLY),
  String(tested.v ?? tested.e).slice(0, 90),
)

// ── 4) 三个模式都必须走同一个模型 ──
const perMode = {}
for (const mode of ['chat', 'qa', 'file']) {
  received.length = 0
  const r = await invoke('chat_with_deepseek', { msg: '你好', mode })
  let reply = ''
  try {
    reply = JSON.parse(String(r.v)).reply ?? ''
  } catch {
    reply = String(r.v ?? '')
  }
  perMode[mode] = {
    ok: r.ok,
    reply,
    hits: received.length,
    path: received[0]?.path ?? '',
    model: received[0]?.body?.model ?? '',
    auth: received[0]?.auth ?? '',
  }
  check(
    `7.${mode}) ${mode} 模式的请求打到了新地址、用了新模型`,
    r.ok && received.length >= 1 && received[0].path.endsWith('/chat/completions') && received[0].body.model === MOCK_MODEL,
    `请求 ${received.length} 次 path=${received[0]?.path ?? '-'} model=${received[0]?.body?.model ?? '-'}`,
  )
  check(`8.${mode}) ${mode} 模式带上了正确密钥且拿到了回复`, received[0]?.auth === `Bearer ${MOCK_KEY}` && reply.includes(MOCK_REPLY), reply.slice(0, 40))
}
check(
  '9) 三个模式用的是同一个模型名（主人要求统一）',
  new Set(['chat', 'qa', 'file'].map((m) => perMode[m].model)).size === 1,
  ['chat', 'qa', 'file'].map((m) => `${m}=${perMode[m].model}`).join(' '),
)
// 文件模式还要带 tools（换服务商不能把工具调用弄丢）
const fileTools = received[0]?.body?.tools?.length ?? 0
check('10) 文件模式的请求仍然带着工具定义', fileTools > 0, `tools=${fileTools}`)

// ── 5) 把主人的原配置放回去（这一步不能省） ──
const restorePayload = {
  llmProvider: original.llmProvider,
  llmBaseUrl: original.llmBaseUrl,
  llmModel: original.llmModel,
  llmApiKey: original.llmApiKey,
}
// 代理开关：原来**没显式设过**就别写进去 —— 保持"未设置 = 按服务商预设"的语义
if (before.llm_use_proxy_set) restorePayload.llmUseProxy = original.llmUseProxy
const restored = await invoke('save_settings', restorePayload)
const after = (await invoke('get_settings')).v || {}
const same =
  (after.llm_provider ?? '') === original.llmProvider.replace(/^$/, 'deepseek') &&
  (after.llm_base_url ?? '') === original.llmBaseUrl &&
  (after.llm_model ?? '') === original.llmModel &&
  (after.llm_api_key ?? '') === original.llmApiKey
check('11) 跑完把主人原来的模型配置原样放回去', restored.ok && same, `现在：provider=${after.llm_provider} model=${after.llm_model_effective}`)

// ── 6) 预设下也能覆盖地址：点「用别的地址」把输入框叫出来 ──
// （走反代 / 镜像站时要用；这条和"选自定义自动露出"是同一段逻辑的两个入口）
await ev(`(() => { const c = document.querySelector('canvas'); const r = c.getBoundingClientRect()
  c.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.width*0.6, clientY: r.height*0.4 })); return true })()`)
await wait(400)
await ev(`(() => { const g = [...document.querySelectorAll('.pie-container svg g')].find((el) => el.querySelector('circle'))
  g.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true })); return true })()`)
await wait(1200)
const overrideFlow = await evalValue(`(() => {
  const before = !!document.querySelector('.settings-panel .llm-base-url')
  const btn = document.querySelector('.settings-panel .llm-link-btn')
  const hadBtn = !!btn
  if (btn) btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
  return { before, hadBtn }
})()`)
await wait(400)
const overrideAfter = await evalValue(`(() => {
  const el = document.querySelector('.settings-panel .llm-base-url')
  return { hasUrl: !!el, placeholder: el?.placeholder || '' }
})()`)
check(
  '12) 预设下点「用别的地址」也能把输入框叫出来（走反代时用）',
  overrideFlow.hadBtn && !overrideFlow.before && overrideAfter.hasUrl,
  `原本收起=${!overrideFlow.before} 有按钮=${overrideFlow.hadBtn} 点完出现=${overrideAfter.hasUrl}`,
)

mock.close()
ws.close()
console.log(fails === 0 ? '\n模型 API 验收全部通过 ✅' : `\n有 ${fails} 项没通过 ❌`)
process.exit(fails === 0 ? 0 : 1)
