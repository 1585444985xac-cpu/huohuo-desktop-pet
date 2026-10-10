<script setup lang="ts">
import { onMounted, onUnmounted, ref, nextTick, computed } from 'vue'
import * as PIXI from 'pixi.js'
import { Live2DModel } from 'pixi-live2d-display/cubism4'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { addChat, getMemoryDetail, deleteMemory, clearMemories, deleteMessage, clearMessages } from './memory'
import type { MemoryDetail } from './memory'

const canvasRef = ref<HTMLCanvasElement | null>(null)

// ─── 闲时碎碎念 ───
const idleBubble = ref('')
const showIdleBubble = ref(false)
const idleBubbleStyle = ref<Record<string, string>>({})
let bubbleTimer: ReturnType<typeof setTimeout> | null = null

const IDLE_THOUGHTS = [
  '呜……今天会不会遇到什么可怕的事……',
  '尾巴大爷又在睡觉了……呼……太好了……',
  '折、折纸人不够了……得赶紧再折几个……',
  '小桂子上次说要拉我去直播……好可怕……',
  '雪衣大人交代的任务还没做完……呜……',
  '嚯嚯嚯嚯嚯嚯嚯！（对着镜子练习笑容）',
  '这、这边好像有什么东西在动……噫！',
  '尾巴救救……啊不对，什么事都没有……',
  '好想回家……但是判官不能早退……',
  '说、说不定……今天也能平安度过……？',
  '（悄悄折了一个新的纸人朋友）',
  '十王司的走廊晚上好黑……我都是跑着过的……',
  '鉴爻轩老板说我是贞凶之命……怎么这样……',
  '熬夜看恐怖片真的能磨砺胆量吗……',
  '呜哇！尾巴大爷你什么时候醒的？！',
]

/** 以宠物头顶为锚点生成气泡/面板定位样式；模型未就绪时返回 null */
function bubbleStyle(topOffset: number): Record<string, string> | null {
  if (!model) return null
  const b = model.getBounds()
  return {
    left: `${b.x + b.width / 2}px`,
    top: `${b.y + topOffset}px`,
    transform: 'translate(-50%, -100%)',
  }
}

function refreshBubbleStyle() {
  const s = bubbleStyle(-18)
  if (s) idleBubbleStyle.value = s
}

function showBubble() {
  if (!model || showChat.value || showIdleBubble.value) {
    bubbleTimer = setTimeout(showBubble, 2000)
    return
  }
  idleBubble.value = IDLE_THOUGHTS[Math.floor(Math.random() * IDLE_THOUGHTS.length)]
  refreshBubbleStyle()
  showIdleBubble.value = true
  bubbleTimer = setTimeout(() => {
    showIdleBubble.value = false
    idleBubble.value = ''
    bubbleTimer = setTimeout(showBubble, 180000 + Math.random() * 60000)
  }, 4000)
}

// ─── 聊天 ───
const chatInput = ref('')
const chatReply = ref('')
const chatLoading = ref(false)
const showChat = ref(false)
const chatStyle = ref<Record<string, string>>({})
const volcEnabled = ref(false)   // 豆包语音开关（默认关、保持安静；用户要用时手动打开）

// ─── 三模式：聊天 / 问答 / 文件 ───
type ChatModeId = 'chat' | 'qa' | 'file'
const MODE_ORDER: ChatModeId[] = ['chat', 'qa', 'file']
const MODE_META: Record<ChatModeId, { label: string; hint: string; placeholder: string }> = {
  chat: {
    label: '💬 聊天',
    hint: '闲聊、说心事，藿藿会记住你',
    placeholder: '跟藿藿说句话吧……我、我会努力的！',
  },
  qa: {
    label: '❓ 问答',
    hint: '直接回答问题，可联网查资料',
    placeholder: '问藿藿一个问题吧……比如「量子纠缠是什么？」',
  },
  file: {
    label: '📁 文件',
    hint: '读写桌面/下载/文档里的文件',
    placeholder: '比如「把桌面上的笔记.txt 整理成要点」',
  },
}
const chatMode = ref<ChatModeId>('chat')
try {
  const saved = localStorage.getItem('huohuo_chat_mode')
  if (saved === 'chat' || saved === 'qa' || saved === 'file') chatMode.value = saved
} catch { /* 忽略隐私模式异常 */ }

function setChatMode(m: ChatModeId) {
  if (chatMode.value === m) return
  chatMode.value = m
  try { localStorage.setItem('huohuo_chat_mode', m) } catch { /* 忽略 */ }
  chatReply.value = `已切换到${MODE_META[m].label}模式。${MODE_META[m].hint}`
}

// ─── 输入框自动增高（向下延展出新输入范围）───
const chatInputRef = ref<HTMLTextAreaElement | null>(null)
let chatInputBaseHeight = 0          // 单行高度（基准）
let chatInputGrow = 0                // 当前高出单行的高度 → 面板底边同步下移

/** 量出单行基准高度：临时清空内容测量，再还原（保证与内容无关） */
function measureInputBase() {
  const el = chatInputRef.value
  if (!el) return
  const saved = el.value
  el.value = ''
  el.style.height = 'auto'
  // 与 autoGrowInput 的算法保持一致（含边框与 1px 取整补偿），否则初次会出现 1px 偏移
  chatInputBaseHeight = el.scrollHeight + (el.offsetHeight - el.clientHeight) + 1
  el.value = saved
  autoGrowInput()
}

/**
 * 按内容自动加行，不设固定行数上限 —— 只要窗口还有空间就一直往下长。
 *
 * 两个避坑点（都实测过）：
 * 1. 全局样式用 box-sizing: border-box，所 scrollHeight 不含边框。
 *    直接 height = scrollHeight 会少 2px，导致永远挂着一条滚动条。
 *    这里用 offsetHeight - clientHeight 取回边框高度补上。 
 * 2. 只有输入框底边要越过窗口底部时才封顶（否则超出部分被窗口裁掉、点不到），
 *    此时才退回框内滚动，正常输入不会出现滚动条。
 */
function autoGrowInput() {
  const el = chatInputRef.value
  if (!el) return
  const top = el.getBoundingClientRect().top
  const available = Math.max(80, window.innerHeight - top - 12)
  el.style.height = 'auto'
  const borderY = el.offsetHeight - el.clientHeight
  // +1px 抵消 scrollHeight/clientHeight 各自的取整误差，确保永不溢出
  const wanted = Math.min(el.scrollHeight + borderY + 1, available)
  el.style.height = `${wanted}px`
  if (!chatInputBaseHeight) chatInputBaseHeight = wanted
  const grow = Math.max(0, wanted - chatInputBaseHeight)
  if (grow !== chatInputGrow) {
    chatInputGrow = grow
    updateChatPosition()
    updateWindowRegion()
  }
}

// ─── 记忆查看 ───
const showMemory = ref(false)
const memoryDetail = ref<MemoryDetail | null>(null)
const memoryLoading = ref(false)
const memoryError = ref('')
const memoryStyle = ref<Record<string, string>>({})
// 记忆面板的定位/拖动统一由下面的 createPanelDrag 接管（见"面板拖动"一节）

// ─── 设置面板（环形菜单中央的设置按钮） ───
const showSettings = ref(false)
const settingsStyle = ref<Record<string, string>>({})
const settingsSaving = ref(false)
const settingsMsg = ref('')

// ─── 上手说明面板（环形菜单第 8 格「说明」） ───
// 饼图本来就是 8 格（NUM_SECTIONS = 8），原来只放了 7 项，左上方那格一直空着。
// 空着不只是难看：新用户上手全靠猜（单击/双击/右键/滚轮/连点四下各是什么，
// 三种模式有什么区别，手机怎么遥控）。这一格就用来回答这些问题。
const showHelp = ref(false)
const helpStyle = ref<Record<string, string>>({})

// ─── 更新对话框（版本更新那一节用） ───
// 样式 ref 必须在这一批声明（下面的 createPanelDrag 建的时候就要用），
// 而它对应的状态在本文件靠后的"版本更新"一节里。
const updateStyle = ref<Record<string, string>>({})
/** 设置文件路径（面板底部的"已从…读取 用） */
const settingsFile = ref('')
const showApiKey = ref(false)
const showVolcToken = ref(false)
const showTgToken = ref(false)
/** 邮箱授权码的显示/隐藏（和别的凭据一样，默认打码） */
const showEmailAuth = ref(false)
/** IMAP 服务器的选择：'auto'（按域名）/ 具体服务器名 / 'custom'（手工填写）
 *
 *  做成下拉而不是输入框：可选的就那 10 台，打字只会打错。
 *  但保留「其他…」—— 企业邮箱和自建邮箱不在表里，没有这条路就永远连不上。*/
const imapChoice = ref('auto')

/** 从已保存的值反推下拉该显示哪一项。放在 loadSettings 里调用 */
function syncImapChoice() {
  const v = (settings.value.imap_server || '').trim()
  if (!v) {
    imapChoice.value = 'auto'
    return
  }
  const known = (settings.value.imap_choices || []).some(c => c.server === v)
  imapChoice.value = known ? v : 'custom'
}

/** 下拉变了 → 同步回设置字段（保存时才会落盘） */
function onImapChoiceChange() {
  if (imapChoice.value === 'custom') {
    // 从已知服务器切到「其他」时清空输入框：
    // 留着上一个服务器的名字会让人以为改了、其实没改
    if ((settings.value.imap_choices || []).some(c => c.server === settings.value.imap_server)) {
      settings.value.imap_server = ''
    }
    return
  }
  settings.value.imap_server = imapChoice.value === 'auto' ? '' : imapChoice.value
}

/** 邮箱「测试连接」的状态 */
const emailTesting = ref(false)
const emailTestMsg = ref('')
const emailTestBad = ref(false)
/** 与后端 settings.rs 的 snapshot() 对应；字段名必须一致 */
const settings = ref({
  deepseek_api_key: '',
  /** 是否已配置密钥（后端快照给的）。没配时不允许静默失败，要在界面上说清楚 */
  has_api_key: true,
  /**
   * 🤖 模型 API：聊天 / 问答 / 文件**三个模式共用**的一份配置。
   * 服务商预设表由后端下发（settings.rs 的 LLM_PROVIDERS），前端不复制这张表 ——
   * 两边各写一份，加了服务商就会忘改一边。
   */
  llm_provider: 'deepseek',
  llm_label: 'DeepSeek（默认）',
  /** 用户填的原文（空 = 用预设默认） */
  llm_base_url: '',
  /** 真正会用的地址与模型（后端算好的，给"当前生效"那行用） */
  llm_base_url_effective: '',
  llm_endpoint: '',
  llm_model: '',
  llm_model_effective: '',
  llm_api_key: '',
  llm_use_proxy: false,
  llm_use_proxy_set: false,
  llm_providers: [] as Array<{ id: string; label: string; base_url: string; model: string; models: string[]; use_proxy: boolean }>,
  llm_provider_default: 'deepseek',
  llm_summary: '',
  /** 豆包（火山引擎）语音凭据 */
  volc_app_id: '',
  volc_access_token: '',
  has_volc_credentials: false,
  volc_enabled: false,
  /** Telegram Bot Token（换 bot 只改这里） */
  telegram_token: '',
  has_telegram_token: false,
  weather_location: '',
  /** 设置文件路径（后端给的，用于在面板上显示"值是从哪读来的" */
  settings_file: '',
  /** 程序版本号 */
  version: '',
  pet_scale: 0.0468,
  pet_scale_default: 0.0468,
  pet_scale_min: 0.02,
  pet_scale_max: 0.1,
  /** 文件模式用的模型（可选值由后端给，见 settings.rs 的 FILE_MODELS） */
  file_model: 'deepseek-v4-pro',
  file_models: ['deepseek-v4-pro', 'deepseek-flash'],
  file_model_default: 'deepseek-v4-pro',
  /** 文件模式一轮里最多跑多少步工具 */
  max_rounds: 10,
  max_rounds_default: 10,
  max_rounds_min: 3,
  max_rounds_max: 30,
  /** 收件邮箱：地址 + IMAP 授权码 + 可选的服务器覆盖 */
  email_addr: '',
  email_auth: '',
  imap_server: '',
  /** 下拉选项（服务器是固定的那几台，由后端给，前端不复制这张表） */
  imap_choices: [] as Array<{ server: string; label: string }>,
  /** 实际会用的服务器（后端按域名推断或取手工填的），给面板做提示 */
  imap_server_effective: '',
  has_email_config: false,
  /** 按域名能认出来的服务器；null = 认不出来，必须手工填 */
  imap_known: null as string | null,
  autostart_pet: false,
  autostart_watchdog: false,
  /**
   * 环形菜单的三个网址快捷键（名称 + 网址 + 是否已配）。
   * 顺序 = 饼图上的顺序：第 4/5/6 格。
   * 第 2 格原来是「漂浮」开关，现在改成第三个网址位（默认空，点它会提示去设置里填）。
   */
  links: [] as Array<{ name: string; url: string; has_url: boolean }>,
  /** 名称长度上限（饼图一格只有 45°，太长会糊；后端也会截断，这里用于提示） */
  link_name_max: 5,
  /**
   * 本地程序位置：QQ / 酷狗。留空 = 自动查找（常见路径 → 注册表 → 开始菜单 → 全盘搜）。
   * 支持把桌面上的快捷方式直接拖进输入框，会自动解析成它指向的 .exe。
   */
  qq_path: '',
  kugou_path: '',
  /** 更新清单地址（空 = 用程序内置的默认值） */
  update_url: '',
  /** 启动后静默检查一次更新（查到才弹窗） */
  update_auto_check: true,
  /** 全局快捷键（空 = 不注册）。在设置里点一下输入框、按下组合键即可 */
  hotkey_chat: '',
  hotkey_toggle: '',
  hotkey_action: '',
  /**
   * 和风天气凭据。**不回传私钥种子**（后端只回传"配没配"）——
   * 面板上那个种子输入框是"要改的时候填一次"，不回显。
   */
  qweather_host: '',
  qweather_project_id: '',
  qweather_credential_id: '',
  has_qweather: false,
  /** 私钥是否已保存（只写字段，靠它显示"还在"） */
  has_qweather_seed: false,
  /** 私钥的提示形式（头尾各 4 位），用于让用户确认"这是我的那把" */
  qweather_seed_hint: '',
  /** 只在本次打开面板时填过才提交（避免用空值把已存的种子清掉） */
  qweather_key_seed: '',
  /** 主人自定义的性格（空 = 内置人设） */
  persona: '',
  persona_max: 1000,
})

/** 文件模式正在做什么（"正在读取 简历.docx"））
 *
 *  以前这几轮里界面**完全没有动静**（只有天气/记忆有 loading），
 *  一趟十几秒的长活儿看上去就像卡死了，用户只能反复点。 */
const fileProgress = ref('')
let unlistenFileProgress: (() => void) | null = null
/** 更新包下载进度监听 */
let unlistenUpdate: (() => void) | null = null
/** 全局快捷键事件监听 */
let unlistenHotkey: (() => void) | null = null

/** 文件模式的项目记忆条数（设置面板里显示，顺便当「清空"入口」 */
const projectMemoryCount = ref(0)

async function refreshProjectMemory() {
  try {
    const info = await invoke<any>('project_memory_info')
    projectMemoryCount.value = Number(info?.entries) || 0
  } catch { /* 后端未就绪 */ }
}

async function clearProjectMemory() {
  try {
    await invoke<number>('project_memory_clear')
    projectMemoryCount.value = 0
    settingsMsg.value = '✅ 项目记忆已清空，她会从这个项目从零开始'
    setTimeout(() => { if (settingsMsg.value.startsWith('✅ 项目记忆')) settingsMsg.value = '' }, 2500)
  } catch (e) {
    settingsMsg.value = `❌ 清空失败：{String(e).slice(0, 120)}`
  }
}

// ─── 面板拖动（设置 / 聊天 / 记忆共用一套） ───
//
// 三个面板除了"默认位置怎么算"以外完全一样。不复制三遍是有原因的：
// 里面有一个**必须一起做的**细节 —— 拖动期间窗口不能裁剪点击区域。
// 否则指针一掉出裁剪区，系统就不再往窗口送事件，拖动会在半路卡死。
// 复制三遍的话，这条迟早会在某一处被漏掉，而且症状是「偶发拖不动」，极难查。

/** 是否正在拖动某个面板。拖动期间窗口不裁剪点击区域，见 setFullWindowRegion */
let panelDragging = false

/** 把面板位置夹进窗口内。
 *
 *  面板浮在透明窗口上，一旦被拖到窗口外面就**再也点不到了**
 *  （窗口的点击区域只覆盖宠物和面板本身），所以必须夹住。 */
function clampPanelPos(
  p: { left: number; top: number },
  panelW: number,
  panelH: number,
) {
  const W = window.innerWidth
  const H = window.innerHeight
  const left = Math.max(0, Math.min(Math.max(0, W - panelW), p.left))
  const top = Math.max(0, Math.min(Math.max(0, H - panelH), p.top))
  return { left: Math.round(left), top: Math.round(top) }
}

/** 拖动期间把**整个窗口**设为可点击。
 *
 *  这条不是优化，是必须的：窗口平时是裁剪过的（只有宠物和面板那一块能接收鼠标），
 *  而面板被拖到新位置时那块区域还没跟上 —— 指针一旦掉出"裁剪区，
 *  系统就不再往窗口里送事件，拖动会在半路卡死。
 *  饼图展开时用的是同一招（见 showPieMenu）。 */
function setFullWindowRegion() {
  const dpr = window.devicePixelRatio || 1
  invoke('set_region', {
    x: 0, y: 0,
    w: Math.round(window.innerWidth * dpr),
    h: Math.round(window.innerHeight * dpr),
  })
}

/** 造一个面板的拖动控制器。
 *
 *  `pos` 为 null 表示"还没拖过"，位置由 `auto()` 跟着桌宠算；
 *  一旦拖过就固定住 —— 不然桌宠一动、窗口一变化就又跳回去，等于白拖。 */
function createPanelDrag(cfg: {
  /** localStorage 键，拖动位置存在这里（重启后还在原处） */
  storageKey: string
  /** 面板元素选择器，拖动时量它的实际尺寸。 */
  selector: string
  pos: { value: { left: number; top: number } | null }
  style: { value: Record<string, string> }
  /** 算默认位置（跟随桌宠） */
  auto: () => Record<string, string> | null
}) {
  function load() {
    try {
      const raw = localStorage.getItem(cfg.storageKey)
      if (!raw) return null
      const p = JSON.parse(raw)
      // 坏数据（手改过 / 旧版本格式）当作没存过，而不是让面板飞到屏幕外
      if (typeof p?.left === 'number' && typeof p?.top === 'number'
          && isFinite(p.left) && isFinite(p.top)) {
        return { left: p.left, top: p.top }
      }
    } catch { /* 忽略 */ }
    return null
  }

  function save() {
    try {
      if (cfg.pos.value) {
        localStorage.setItem(cfg.storageKey, JSON.stringify(cfg.pos.value))
      } else {
        localStorage.removeItem(cfg.storageKey)
      }
    } catch { /* 忽略 */ }
  }

  /** 刷新 style。没拖过用 auto()，拖过就用拖到的坐标 */
  function position() {
    const base = cfg.auto()
    if (!base) return
    if (!cfg.pos.value) {
      cfg.style.value = base
      return
    }
    // 只保留 auto() 里**非定位**的键（比如 maxHeight），定位一律用拖到的坐标。
    // 聊天面板原本靠 transform 锚在宠物头顶，拖过之后必须把 transform 关掉。
    // 否则它会一边按 left/top 走、一边又被锚点拽回去，永远差半个身位。
    const { left: _l, top: _t, transform: _tf, ...rest } = base
    const el = document.querySelector(cfg.selector) as HTMLElement | null
    const r = el?.getBoundingClientRect()
    const p = clampPanelPos(cfg.pos.value, r?.width ?? 320, r?.height ?? 420)
    cfg.style.value = { ...rest, left: `${p.left}px`, top: `${p.top}px`, transform: 'none' }
  }

  /** 启动时把上次拖到的位置读回来 */
  function restore() {
    cfg.pos.value = load()
  }

  /** 按住标题栏拖动 */
  function onDragStart(e: PointerEvent) {
    if (e.button !== 0) return
    // 标题栏上一般还有关闭/记忆等按钮，别把它们变成拖动把手
    if ((e.target as HTMLElement).closest('button')) return

    const handle = e.currentTarget as HTMLElement
    const panel = handle.closest(cfg.selector) as HTMLElement | null
    if (!panel) return

    const r = panel.getBoundingClientRect()
    // 记下抓取点相对面板左上角的偏移：不这样的话面板会"跳"到指针正下方
    const offX = e.clientX - r.left
    const offY = e.clientY - r.top

    panelDragging = true
    setFullWindowRegion()
    // setPointerCapture 拿不到指针时会**抛异常**（比如指针已经抬起）。
    // 不包起来的话整个拖动会在这里中断，表现就是「标题栏拖不动" —」 
    // 所以它是尽力而为：拿不到捕获也还能靠 window 上的监听拖。
    try { handle.setPointerCapture(e.pointerId) } catch { /* 拿不到就算了 */ }
    e.preventDefault()

    // 只有**真的移动过**才落位。
    // 否则单击一下标题栏（比如想看看提示）就会把"跟随桌宠"悄悄变成
    // "固定在这个坐标」，之后再也不会跟着桌宠动了 —— 用户完全不知道发生了什么。
    let moved = 0
    let lx = e.clientX
    let ly = e.clientY

    const onMove = (ev: PointerEvent) => {
      if (!panelDragging) return
      moved += Math.abs(ev.clientX - lx) + Math.abs(ev.clientY - ly)
      lx = ev.clientX
      ly = ev.clientY
      if (moved < 3) return
      cfg.pos.value = clampPanelPos(
        { left: ev.clientX - offX, top: ev.clientY - offY },
        r.width,
        r.height,
      )
      position()
    }

    const finish = (ev: PointerEvent) => {
      panelDragging = false
      try { handle.releasePointerCapture(ev.pointerId) } catch { /* 本来就没捕获到 */ }
      // 挂在 window 上（而不是标题栏）：标题栏只有二三十像素高，
      // 捕获一旦没拿到，指针往上一移就出了它的范围，拖动会当场断掉
      window.removeEventListener('pointermove', onMove, true)
      window.removeEventListener('pointerup', finish, true)
      window.removeEventListener('pointercancel', finish, true)
      if (moved >= 3) save()
      updateWindowRegion() // 恢复裁剪
    }

    window.addEventListener('pointermove', onMove, true)
    window.addEventListener('pointerup', finish, true)
    window.addEventListener('pointercancel', finish, true)
  }

  /** 双击标题栏：忘掉拖动位置，回到跟随桌宠 */
  function reset() {
    cfg.pos.value = null
    save()
    position()
    updateWindowRegion()
  }

  return { position, restore, onDragStart, reset }
}

// ── 设置面板 ──
/** 设置面板被拖到的位置；null = 还没拖过，跟随桌宠自动摆（默认） */
const settingsPos = ref<{ left: number; top: number } | null>(null)

function settingsAutoStyle(): Record<string, string> | null {
  if (!model) return null
  const W = window.innerWidth
  const H = window.innerHeight
  const panelW = 340
  const maxH = Math.min(H - 60, 560)
  const b = model.getBounds()
  const cx = b.x + b.width / 2
  const cy = b.y + b.height * 0.35
  const left = Math.max(10, Math.min(W - panelW - 10, cx - panelW / 2))
  const top = Math.max(10, Math.min(H - maxH - 10, cy - maxH / 2))
  return { left: `${left}px`, top: `${top}px`, maxHeight: `${maxH}px` }
}

const settingsDrag = createPanelDrag({
  storageKey: 'huohuo_settings_pos',
  selector: '.settings-panel',
  pos: settingsPos,
  style: settingsStyle,
  auto: settingsAutoStyle,
})
settingsDrag.restore()

function updateSettingsPosition() { settingsDrag.position() }
function onSettingsDragStart(e: PointerEvent) { settingsDrag.onDragStart(e) }
function resetSettingsPos() { settingsDrag.reset() }

// ── 上手说明面板 ──
/** 说明面板被拖到的位置；null = 还没拖过，跟随桌宠自动摆（默认） */
const helpPos = ref<{ left: number; top: number } | null>(null)

const helpDrag = createPanelDrag({
  storageKey: 'huohuo_help_pos',
  selector: '.help-panel',
  pos: helpPos,
  style: helpStyle,
  // 尺寸和设置面板一致（都是 340 宽），直接复用它的自动摆位：贴在桌宠头顶
  auto: settingsAutoStyle,
})
helpDrag.restore()

function updateHelpPosition() { helpDrag.position() }
function onHelpDragStart(e: PointerEvent) { helpDrag.onDragStart(e) }
function resetHelpPos() { helpDrag.reset() }

// ── 更新对话框 ──
const updatePos = ref<{ left: number; top: number } | null>(null)

const updateDrag = createPanelDrag({
  storageKey: 'huohuo_update_pos',
  selector: '.update-panel',
  pos: updatePos,
  style: updateStyle,
  auto: settingsAutoStyle,
})
updateDrag.restore()

function onUpdateDragStart(e: PointerEvent) { updateDrag.onDragStart(e) }
function resetUpdatePos() { updateDrag.reset() }

// ── 聊天面板 ──
const chatPos = ref<{ left: number; top: number } | null>(null)

const chatDrag = createPanelDrag({
  storageKey: 'huohuo_chat_pos',
  selector: '.chat-panel',
  pos: chatPos,
  style: chatStyle,
  // 默认位置：底边对齐宠物头顶上方 20px；输入框每长高一点，底边就往下让一点，
  // 这样新增的输入范围是向下延展的（而不是把整个面板往上顶）
  auto: () => bubbleStyle(-20 + chatInputGrow),
})
chatDrag.restore()

function updateChatPosition() { chatDrag.position() }
function onChatDragStart(e: PointerEvent) { chatDrag.onDragStart(e) }
function resetChatPos() { chatDrag.reset() }

// ── 记忆面板 ──
const memoryPos = ref<{ left: number; top: number } | null>(null)

function memoryAutoStyle(): Record<string, string> | null {
  if (!model) return null
  // 记忆面板较大，居中覆盖藿藿，上下留出边距
  const W = window.innerWidth
  const H = window.innerHeight
  const b = model.getBounds()
  const cx = b.x + b.width / 2
  const cy = b.y + b.height * 0.35  // 偏向藿藿上半身
  const panelW = 320
  const maxH = Math.min(H - 60, 500)  // 上下各留 30px
  const left = Math.max(10, Math.min(W - panelW - 10, cx - panelW / 2))
  const top = Math.max(10, Math.min(H - maxH - 10, cy - maxH / 2))
  return { left: `${left}px`, top: `${top}px`, maxHeight: `${maxH}px` }
}

const memoryDrag = createPanelDrag({
  storageKey: 'huohuo_memory_pos',
  selector: '.memory-panel',
  pos: memoryPos,
  style: memoryStyle,
  auto: memoryAutoStyle,
})
memoryDrag.restore()

function updateMemoryPosition() { memoryDrag.position() }
function onMemoryDragStart(e: PointerEvent) { memoryDrag.onDragStart(e) }
function resetMemoryPos() { memoryDrag.reset() }

/** 桌宠大小的落盘防抖：滚轮一次滚动会产生十几个事件，
 *  每次都写文件既没必要也容易撞上别的写入。等停手 600ms 再写一次。 */
let petScaleSaveTimer: ReturnType<typeof setTimeout> | null = null
function persistPetScaleSoon() {
  if (petScaleSaveTimer) clearTimeout(petScaleSaveTimer)
  petScaleSaveTimer = setTimeout(() => {
    petScaleSaveTimer = null
    const v = settings.value.pet_scale
    if (!Number.isFinite(v)) return
    invoke('save_settings', { petScale: v })
      .then(() => {
        // 与基准同步：这样之后在设置面板里按保存，不会把同一个值又提交一遍
        settingsBaseline.pet_scale = v
      })
      .catch(() => { /* 后端未就绪时不影响本次缩放 */ })
  }, 600)
}

/**
 * 打开面板时后端快照的内容。保存时**只提交被改过的字段**。
 *
 * 这不是优化，是防错：面板刚渲染出来时各字段还是默认值（空）。 
 * 若整份提交，就会用空串把用户已存好的凭据覆盖掉。 
 * —— 这个坑真的踩过：火山引擎的 App ID / Token 被这么清空过一次。
 */
let settingsBaseline: Record<string, any> = {}
/** 设置是否已经从后端加载完成。没完成就不允许保存 */
const settingsLoaded = ref(false)

/**
 * 给"基准快照"用的深拷贝。
 *
 * `links` 是数组：浅拷贝（`{ ...settings.value }`）会让基准和界面共用同一个数组，
 * 于是面板里改个名字就把基准一起改了 —— diff 永远看不到改动，用户按保存只会得到
 * 一句"没有改动"。其它字段是标量，浅拷贝就够。
 */
function snapshotSettings(): Record<string, any> {
  const s: Record<string, any> = { ...settings.value }
  s.links = (settings.value.links || []).map((l) => ({ ...l }))
  return s
}

async function loadSettings() {
  try {
    const s = await invoke<any>('get_settings')
    if (s) {
      settings.value = { ...settings.value, ...s }
      settingsBaseline = snapshotSettings()
      settingsFile.value = s.settings_file || ''
      settingsLoaded.value = true
      // 记住当前服务商：换服务商时要判断"地址/模型还是不是上一家的默认值"
      lastLlmProvider.value = settings.value.llm_provider
      // 服务器的下拉选项要跟着已保存的值对应
      syncImapChoice()
    }
  } catch {
    // 后端还没就绪时保持「未加载」，免得用户一按保存就把未知字段当空值写掉。
    settingsLoaded.value = false
  }
}

function openSettings() {
  hidePieMenu()
  // **先加载再显示**：面板一旦先显示出来，各字段就是默认值（空）。 
  // 用户在加载完成前按保存就会把磁盘上的真实值覆盖掉。 
  // 宁可晚几十毫秒出现，也不能显示一份「看起来是空的"设置」。
  loadSettings().then(() => {
    if (!settingsLoaded.value) {
      chatReply.value = '⚠️ 读取设置失败，请稍后再试（设置文件可能被占用）'
      return
    }
    settingsMsg.value = ''
    showSettings.value = true
    // 面板一出现就停下正在进行的漂浮：否则它带着面板一路飘走
    stopFloat()
    updateSettingsPosition()
    updateWindowRegion()
    refreshProjectMemory()
    // 版本/构建时间也刷一次（都是本机状态，读起来不值钱）
    loadUpdateInfo()
  })
}

function toggleSettings() {
  if (showSettings.value) { showSettings.value = false } else { openSettings() }
  updateWindowRegion()
}

/** 打开上手说明：不需要读盘，直接显示（和设置面板不同，它没有"先加载再显示"的必要） */
function openHelp() {
  hidePieMenu()
  showHelp.value = true
  updateHelpPosition()
  updateWindowRegion()
}

function toggleHelp() {
  if (showHelp.value) { showHelp.value = false } else { openHelp() }
  updateWindowRegion()
}

/** 桌宠大小：拖动时立即生效（实时预览），松开 保存时才落盘 */
function applyPetScale(v: number) {
  if (!model) return
  // 非数字直接忽略：滑块值一旦被污染成 NaN，界面会空掉。
  // 而且 NaN 序列化成 null 后后端也救不回来（这个坑真的踩过）
  if (!Number.isFinite(v)) return
  const min = Number(settings.value.pet_scale_min) || 0.02
  const max = Number(settings.value.pet_scale_max) || 0.1
  const s = Math.max(min, Math.min(max, v))
  settings.value.pet_scale = s
  model.scale.set(s)
  updateWindowRegion()
}

async function saveSettings(): Promise<boolean> {
  // 没加载完就保存 = 拿默认值（空串）覆盖磁盘上的真实值。直接拒绝。
  if (!settingsLoaded.value) {
    settingsMsg.value = '❌ 设置还没加载完，稍等一下再保存'
    return false
  }

  // 只提交改动过的字段：没动过的（尤其是凭据）一律不提交。
  // 这样即使界面上某个字段因为任何原因是空的，也不会把磁盘上的值清掉
  const cur = settings.value as Record<string, any>
  const payload: Record<string, unknown> = {}
  const diff = (key: string, argName: string) => {
    if (cur[key] !== settingsBaseline[key]) payload[argName] = cur[key]
  }
  // 🤖 模型 API：五个字段都能改（服务商 / 地址 / 模型 / 密钥 / 代理）。
  // 密钥在这里是**普通字段**（面板本来就回显它，和只写的和风私钥不一样）
  diff('llm_provider', 'llmProvider')
  diff('llm_base_url', 'llmBaseUrl')
  diff('llm_model', 'llmModel')
  diff('llm_api_key', 'llmApiKey')
  diff('llm_use_proxy', 'llmUseProxy')
  diff('volc_app_id', 'volcAppId')
  diff('volc_access_token', 'volcAccessToken')
  diff('volc_enabled', 'volcEnabled')
  diff('telegram_token', 'telegramToken')
  diff('weather_location', 'weatherLocation')
  diff('pet_scale', 'petScale')
  diff('file_model', 'fileModel')
  diff('max_rounds', 'maxRounds')
  diff('email_addr', 'emailAddr')
  diff('email_auth', 'emailAuth')
  diff('imap_server', 'imapServer')
  diff('qq_path', 'qqPath')
  diff('kugou_path', 'kugouPath')
  diff('update_url', 'updateUrl')
  diff('update_auto_check', 'updateAutoCheck')
  diff('hotkey_chat', 'hotkeyChat')
  diff('hotkey_toggle', 'hotkeyToggle')
  diff('hotkey_action', 'hotkeyAction')
  diff('qweather_host', 'qweatherHost')
  diff('qweather_project_id', 'qweatherProjectId')
  diff('qweather_credential_id', 'qweatherCredentialId')
  diff('persona', 'persona')
  // 私钥种子只在"这次真的填了"时才提交：面板不回显它，
  // 若把它当成普通字段 diff，一次保存就会把磁盘上已有的种子写成空
  if (cur.qweather_key_seed !== '') payload.qweatherKeySeed = cur.qweather_key_seed
  // 网址快捷键：界面是一个数组，后端是 link1_name / link1_url … 六个字段。
  // 逐格比对，只有动过的格子才提交（和其它字段同一个规矩：没动就不提交）
  const baseLinks = (settingsBaseline.links || []) as Array<{ name: string; url: string }>
  settings.value.links.forEach((l, i) => {
    const b = baseLinks[i] || { name: '', url: '' }
    if (l.name !== b.name) payload[`link${i + 1}Name`] = l.name
    if (l.url !== b.url) payload[`link${i + 1}Url`] = l.url
  })

  if (Object.keys(payload).length === 0) {
    settingsMsg.value = '没有改动'
    setTimeout(() => { if (settingsMsg.value === '没有改动') settingsMsg.value = '' }, 2000)
    // 没改动也算成功：磁盘上的就是用户现在看到的这份
    return true
  }

  settingsSaving.value = true
  settingsMsg.value = ''
  try {
    await invoke('save_settings', payload)
    // 保存成功 → 当前状态成为新的基准（links 要深拷贝，理由见 snapshotSettings）
    settingsBaseline = snapshotSettings()
    // 天气地点同时同步到悬浮弹窗（两处共用一个来源）
    weatherAddress.value = settings.value.weather_location
    try { localStorage.setItem(WEATHER_LOCATION_KEY, settings.value.weather_location) } catch { /* 忽略 */ }
    // 密钥填了就把警告撤掉（不用重新拉一次设置）
    settings.value.has_api_key = settings.value.llm_api_key.trim().length > 0
    settings.value.has_volc_credentials =
      settings.value.volc_app_id.trim().length > 0 && settings.value.volc_access_token.trim().length > 0
    settings.value.has_telegram_token = settings.value.telegram_token.trim().length > 0
    // 邮箱算是"配好了"要地址和授权码都在，且服务器能定下来（按域名认出或手工填了）。
    // 这是前端自己的判断，只用来决定轮询开关；真正的判定在后端 email_config()
    settings.value.has_email_config =
      settings.value.email_addr.trim().length > 0 &&
      settings.value.email_auth.trim().length > 0 &&
      (!!settings.value.imap_server.trim() || !!settings.value.imap_known)
    // 保存后重新拉一次后端算出来的服务器名，提示文字才跟得上
    loadSettings()
    // 饼图上的三个名字也要跟着变（它读的是单独一份轻量快照）
    loadLinkShortcuts()
    // 快捷键改了就重新注册（后端线程 500 毫秒内生效，然后取回结果）
    applyHotkeys()
    // 语音开关跟着设置走（聊天框那个按钮读的是同一个 ref）
    volcEnabled.value = settings.value.volc_enabled
    settingsMsg.value = '✅ 已保存'
    setTimeout(() => { if (settingsMsg.value === '✅ 已保存') settingsMsg.value = '' }, 2000)
    return true
  } catch (e) {
    // 注意 $ 不能漏：漏了的话界面上会原样显示 "{String(e).slice(0, 120)}"，
    // 用户看到一个模板字符串，而不是真正的失败原因
    settingsMsg.value = `❌ 保存失败：${String(e).slice(0, 120)}`
    return false
  } finally {
    settingsSaving.value = false
  }
}

/** 网址快捷键的「↗ 先试试」：用输入框里的值打开一次，不必先保存 */
async function testLink(url: string) {
  const u = (url || '').trim()
  if (!u) return
  try {
    await invoke('open_link_slot', { slot: 0, url: u })
  } catch (e) {
    settingsMsg.value = `❌ ${String(e).slice(0, 120)}`
  }
}

/** 程序位置的「↗ 试一下」：按框里的位置启动一次（酷狗会连带开始播放，约 8 秒） */
async function testProgram(key: 'qq_path' | 'kugou_path') {
  settingsMsg.value = '⏳ 正在启动…'
  try {
    const msg = key === 'qq_path'
      ? await invoke<string>('open_qq')
      : await invoke<string>('open_kugou_music')
    settingsMsg.value = `✅ ${msg}`
  } catch (e) {
    settingsMsg.value = `❌ ${String(e).slice(0, 120)}`
  }
  setTimeout(() => { if (settingsMsg.value.startsWith('✅') || settingsMsg.value.startsWith('❌')) settingsMsg.value = '' }, 5000)
}

// ─── 版本更新 ───
//
// 一键式的"检查 → 弹窗 → 下载 → 安装"。像正经软件那样：
// 查到新版本**自动弹出一个对话框**（当前版本 → 新版本、更新说明、大小），
// 点「立即更新」就地显示进度条，装完自动重启。
//
// 关键判断都在后端（update.rs）：清单要**验签**才认，下载完要核对 sha256；
// 这里只负责把结论显示清楚 —— 尤其是失败原因，静默失败在更新这件事上最要命。

const updateInfo = ref<{ version: string; build_epoch: number; installed: boolean; update_url: string; auto_check: boolean }>({
  version: '',
  build_epoch: 0,
  installed: false,
  update_url: '',
  auto_check: true,
})

const updateBusy = ref(false)
const updateMsg = ref('')
const updateBad = ref(false)
/** 更新说明（纯展示，不参与签名） */
const updateNotes = ref('')

// ── 更新对话框（查到新版本时自动弹出） ──
const updateDialog = ref(false)
// 注意：updateStyle 在上面（和别的面板样式放一起）声明 —— createPanelDrag 建的时候就要用
const updateFound = ref<{ version: string; version_code: number; notes: string; size: number }>({
  version: '',
  version_code: 0,
  notes: '',
  size: 0,
})
/** '' | 'manifest' | 'download' | 'verify' | 'ready' | 'install' */
const updatePhase = ref('')
const updatePercent = ref(0)
const updateGot = ref(0)
const updateTotal = ref(0)
/** 下载速度（由相邻两次进度事件算出来） */
const updateSpeed = ref('')
/** 算速度用的上一次采样（后端不必为此存状态） */
let updateLastGot = -1
let updateLastTs = 0
/** 启动后自动检查更新的定时器 */
let updateAutoTimer: ReturnType<typeof setTimeout> | null = null

/** 「跳过这个版本」记在本地（这是界面偏好，不是程序设置，不必进 settings.json） */
const SKIP_UPDATE_KEY = 'huohuo_skip_update_code'
let skippedUpdateCode = 0
try { skippedUpdateCode = Number(localStorage.getItem(SKIP_UPDATE_KEY) || 0) || 0 } catch { /* 忽略 */ }

function formatBytes(n: number): string {
  if (!n) return '0 B'
  if (n < 1024) return `${n} B`
  if (n < 1048576) return `${(n / 1024).toFixed(0)} KB`
  return `${(n / 1048576).toFixed(1)} MB`
}

/** 构建时间戳 → 本地时间人话。Rust 侧只给 epoch，换算交给这里（免得在 Rust 里造时区表） */
function formatBuildTime(epoch: number): string {
  if (!epoch) return '时间未知'
  return new Date(epoch * 1000).toLocaleString('zh-CN', { hour12: false })
}

/**
 * 更新源只显示主机名（`pet.myheart.vip`），不把整条 latest.json 地址摊在面板上 ——
 * 面板那一行放不下长 URL，而用户真正要知道的只是"从哪更新"。
 * 完整地址挂在 title 上，想看能看。
 */
const updateSourceHost = computed(() => {
  const url = updateInfo.value.update_url || ''
  try {
    return new URL(url).host
  } catch {
    return url
  }
})

async function loadUpdateInfo() {
  try {
    updateInfo.value = await invoke<any>('update_status')
  } catch { /* 后端没起来 */ }
}

function openUpdateDialog() {
  updateDialog.value = true
  updateDrag.position()
  updateWindowRegion()
}

function closeUpdateDialog() {
  if (updateBusy.value) return   // 正在下载/安装，别让"稍后"把进度藏起来
  updateDialog.value = false
  updateWindowRegion()
}

/** 跳过这个版本：同一个构建不再自动弹（手动点检查还是能查出来） */
function skipThisVersion() {
  skippedUpdateCode = updateFound.value.version_code
  try { localStorage.setItem(SKIP_UPDATE_KEY, String(skippedUpdateCode)) } catch { /* 忽略 */ }
  updateMsg.value = `已跳过 ${updateFound.value.version}（下次手动检查仍会显示）`
  updateDialog.value = false
  updateWindowRegion()
}

/**
 * 检查更新。`auto = true` 时是启动后的静默检查：没更新就什么都不做，
 * 有更新且没被跳过才弹窗 —— 不能每次开机都拿同一个版本烦人。
 */
async function checkUpdate(auto = false) {
  if (updateBusy.value) return
  updateBusy.value = true
  updateBad.value = false
  updateMsg.value = auto ? '' : '⏳ 正在检查更新…'
  // 顺手刷新一次"当前版本 / 更新源"：这两个值可能被外部改过
  // （比如手工改了 settings.json 里的 update_url），只靠启动那一次会显示成旧的
  loadUpdateInfo()
  try {
    const r = await invoke<any>('update_check')
    updateNotes.value = r.notes || ''
    updateMsg.value = r.has_update
      ? `⬆️ ${r.message}（当前 ${r.current_version} · ${formatBuildTime(r.current_build_epoch)}）`
      : `✅ ${r.message}（当前构建 ${formatBuildTime(r.current_build_epoch)}）`
    if (r.has_update) {
      updateFound.value = {
        version: r.version,
        version_code: r.version_code,
        notes: r.notes || '',
        size: Number(r.size || 0),
      }
      const skip = auto && skippedUpdateCode === r.version_code
      if (!skip) openUpdateDialog()
    }
  } catch (e) {
    // 自动检查失败不打扰（多半只是没网）；手动点才报错
    if (!auto) {
      updateBad.value = true
      updateMsg.value = `❌ ${String(e).slice(0, 200)}`
    }
  } finally {
    updateBusy.value = false
  }
}

async function installUpdate() {
  updateBusy.value = true
  updateBad.value = false
  updateMsg.value = ''
  updatePhase.value = 'manifest'
  updatePercent.value = 0
  updateGot.value = 0
  updateTotal.value = updateFound.value.size
  updateSpeed.value = ''
  try {
    const r = await invoke<any>('update_install')
    updateMsg.value = `${r.mode === 'silent' ? '⬆️' : '⬇️'} ${r.message}`
  } catch (e) {
    updateBad.value = true
    updateMsg.value = `❌ ${String(e).slice(0, 200)}`
    updatePhase.value = ''
  } finally {
    updateBusy.value = false
  }
}

// ─── 全局快捷键 ───
//
// 设置里点一下输入框再按组合键就录进去（不用手打文本，也不会打错）。
// 真正的注册在后端（hotkeys.rs，RegisterHotKey）；这里只负责录制、保存后让它重新注册、
// 以及把"哪一个没注册上、为什么"如实显示出来 —— 热键被别的软件占用时，
// 用户看到的必须是一句原因，而不是"我设了但没反应"。

const hotkeyStatus = ref<Array<{ id: string; label: string; accel: string; ok: boolean; error: string }>>([])
const hotkeyMsg = ref('')
/** 正在录制的输入框（让它显示"请按组合键…"） */
const hotkeyRecording = ref('')

/** 键盘事件 → 后端认得的键名（与 hotkeys.rs 的 parse_key 对应） */
function hotkeyMainKey(e: KeyboardEvent): string {
  const code = e.code || ''
  let m: RegExpMatchArray | null
  if ((m = code.match(/^Key([A-Z])$/))) return m[1]
  if ((m = code.match(/^Digit(\d)$/))) return m[1]
  if ((m = code.match(/^F(\d{1,2})$/))) return m[1] && Number(m[1]) <= 24 ? `F${m[1]}` : ''
  const map: Record<string, string> = {
    Space: 'Space', Enter: 'Enter', NumpadEnter: 'Enter', Tab: 'Tab', Backspace: 'Backspace',
    Delete: 'Delete', Insert: 'Insert', Home: 'Home', End: 'End',
    PageUp: 'PageUp', PageDown: 'PageDown',
    ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right',
  }
  return map[code] || ''
}

/** 录制：按住修饰键再按主键，写进对应的设置项 */
function captureHotkey(e: KeyboardEvent, field: string) {
  e.preventDefault()
  hotkeyRecording.value = field
  const isModifierOnly = ['Control', 'Alt', 'Shift', 'Meta'].includes(e.key)
  if (isModifierOnly) return            // 只按了修饰键：等他接着按主键
  const main = hotkeyMainKey(e)
  if (!main) {
    hotkeyMsg.value = `❌ 这个键（${e.key}）暂时不被支持，换一个试试`
    return
  }
  const parts: string[] = []
  if (e.ctrlKey) parts.push('Ctrl')
  if (e.altKey) parts.push('Alt')
  if (e.shiftKey) parts.push('Shift')
  if (e.metaKey) parts.push('Win')
  ;(settings.value as Record<string, any>)[field] = [...parts, main].join('+')
  hotkeyMsg.value = ''
  hotkeyRecording.value = ''
}

function clearHotkey(field: string) {
  ;(settings.value as Record<string, any>)[field] = ''
  hotkeyMsg.value = ''
}

/** 读一次注册结果（后端热键线程最迟 500 毫秒内重新注册完） */
async function refreshHotkeyStatus() {
  try {
    const s = await invoke<any>('hotkey_status')
    hotkeyStatus.value = s?.items ?? []
  } catch { /* 后端没起来 */ }
}

/** 保存设置之后调用：让后端按新配置重新注册，并把结果取回来 */
async function applyHotkeys() {
  try {
    await invoke('apply_hotkeys')
    await new Promise((r) => setTimeout(r, 700))   // 等热键线程那一轮醒来
    await refreshHotkeyStatus()
  } catch (e) {
    hotkeyMsg.value = `❌ ${String(e).slice(0, 120)}`
  }
}

/** 快捷键动作：隐藏 / 显示桌宠（藏起来之后还能用同一个键叫回来） */
async function togglePetVisible() {
  const w = getCurrentWindow()
  try {
    if (await w.isVisible()) {
      await w.hide()
    } else {
      await w.show()
      await w.setFocus()
    }
  } catch { /* 窗口 API 异常就算了，别影响别的动作 */ }
}

// ─── 拖放：桌面快捷方式 → 「程序位置」输入框 ───

/**
 * 松手那一刻指针底下是哪个拖放目标（输入框上的 `data-drop-target`）。
 *
 * Tauri 的拖放事件给的是**物理坐标**（125% 缩放时是 CSS 像素的 1.25 倍），
 * 所以要先除以 devicePixelRatio 再问 elementFromPoint。
 */
function dropTargetAt(pos?: { x: number; y: number }): string {
  if (!pos) return ''
  const dpr = window.devicePixelRatio || 1
  const el = document.elementFromPoint(pos.x / dpr, pos.y / dpr) as HTMLElement | null
  return el?.closest?.('[data-drop-target]')?.getAttribute('data-drop-target') || ''
}

/**
 * 把拖进来的文件填进对应的输入框。
 *
 * `.lnk` 先交给后端解析成真实路径 —— 用户手里现成的就是桌面上那个图标，
 * 让他自己去翻安装目录是强人所难。丢在别处（不在输入框上）就什么都不做，
 * 免得手一滑就把设置改了。
 */
async function handleFileDrop(paths: string[], pos?: { x: number; y: number }) {
  const key = dropTargetAt(pos)
  const file = (paths || [])[0]
  if (!key || !file) return
  try {
    const real = await invoke<string>('resolve_shortcut', { path: file })
    ;(settings.value as Record<string, any>)[key] = real
    // 页脚只放得下一行：太长的路径显示尾部，完整值在输入框里
    settingsMsg.value = `✅ 已填入：${real.length > 46 ? '…' + real.slice(-46) : real}`
    setTimeout(() => {
      if (settingsMsg.value.startsWith('✅ 已填入')) settingsMsg.value = ''
    }, 5000)
  } catch (e) {
    settingsMsg.value = `❌ ${String(e).slice(0, 120)}`
  }
}

/** 邮箱「测试连接」。
 *
 *  为什么按钮要先保存再测：后端 test_email 读的是**磁盘上的配置**。
 *  不先落盘的话，用户会拿旧配置去测，然后对着一个莫名其妙的失败发呆
 *  —— 而邮箱配错的默认症状本来就是"每隔 60 秒一次静默失败"。
 *  那个反馈已经够差了，测试按钮不能再制造一次。 
 */
async function testEmail() {
  if (!settingsLoaded.value) {
    emailTestBad.value = true
    emailTestMsg.value = '❌ 设置还没加载完，稍等一下再测'
    return
  }
  emailTesting.value = true
  emailTestMsg.value = ''
  try {
    const saved = await saveSettings()
    if (!saved) {
      emailTestBad.value = true
      emailTestMsg.value = `❌ 没能保存，先把上面填的内容改对：${settingsMsg.value}`
      return
    }
    const r = await invoke<string>('test_email')
    emailTestBad.value = false
    emailTestMsg.value = r
  } catch (e: any) {
    // 后端给的是人话（区分"连不上/登录失败/开不了收件箱），原样展示
    emailTestBad.value = true
    emailTestMsg.value = `❌ ${String(e ?? '').slice(0, 300)}`
  } finally {
    emailTesting.value = false
  }
}

/** 当前选中的服务商预设（后端下发的表里查；模板里也用这个） */
const llmProvider = computed(() =>
  (settings.value.llm_providers || []).find((p) => p.id === settings.value.llm_provider),
)

/** 用户点了「用别的地址」—— 预设下也能覆盖地址（走反代 / 镜像站时用） */
const baseUrlOverride = ref(false)

/**
 * 接口地址输入框要不要露出来。
 *
 * 预设都自带默认地址，平时只需要显示"正在用哪一条"，所以把输入框收起来；
 * 三种情况必须露：① 选了「自定义（OpenAI 兼容）」② 用户点了「用别的地址」
 * ③ **已经存着一条自定义地址** —— 藏起一个有值的输入框，等于留了一个看不见的配置，
 * 那正是这个项目最不接受的事（配置必须看得见）。
 */
const showBaseUrlInput = computed(
  () =>
    settings.value.llm_provider === 'custom' ||
    baseUrlOverride.value ||
    !!settings.value.llm_base_url.trim(),
)

/** 上一次选中的服务商 id：用来判断地址/模型是不是"上一家的默认值" */
const lastLlmProvider = ref('')

/**
 * 换服务商：地址与模型**如果还是上一家的默认值（或本来就空）就清掉**，
 * 让新预设的默认值生效；代理开关按新预设的建议重置（用户随后可以自己改）。
 *
 * 为什么必须清：地址是跟着服务商走的 —— 把 OpenAI 的地址留在 DeepSeek 上，
 * 或反过来，只会得到一个 404，而用户完全看不出是自己刚才换服务商换出来的。
 */
function onLlmProviderChange() {
  const prev = (settings.value.llm_providers || []).find((p) => p.id === lastLlmProvider.value)
  const next = llmProvider.value
  const url = settings.value.llm_base_url.trim()
  // 上一家是「自定义」时，它的默认地址是空串 —— 那条地址本来就是"自定义服务商"的一部分，
  // 换到预设上还留着，只会让请求打到一个跟当前服务商无关的地方，所以一并清掉。
  const urlWasInherited = !url || url === prev?.base_url || prev?.id === 'custom'
  if (urlWasInherited) {
    settings.value.llm_base_url = ''
    baseUrlOverride.value = false // 输入框跟着收回去，只显示"正在用哪条默认地址"
  }
  const model = settings.value.llm_model.trim()
  if (!model || model === prev?.model) settings.value.llm_model = ''
  settings.value.llm_use_proxy = next?.use_proxy ?? false
  lastLlmProvider.value = settings.value.llm_provider
  llmTestMsg.value = ''
}

/** 模型 API「测试连接」的状态 */
const llmTesting = ref(false)
const llmTestMsg = ref('')
const llmTestOk = ref(false)

/**
 * 测一次连接。先保存再测：后端测的是**设置文件里**的配置，
 * 不保存就等于在测上一份配置（用户会以为新填的能用）。
 * 失败信息由后端分类成人话（401 指密钥/服务商、404 指地址、400 指模型名）。
 */
async function testLlm() {
  if (!settingsLoaded.value) {
    llmTestOk.value = false
    llmTestMsg.value = '❌ 设置还没加载完，稍等一下再测'
    return
  }
  llmTesting.value = true
  llmTestMsg.value = ''
  try {
    const saved = await saveSettings()
    if (!saved) {
      llmTestOk.value = false
      llmTestMsg.value = `❌ 没能保存，先把上面填的内容改对：${settingsMsg.value}`
      return
    }
    const r = await invoke<string>('test_llm_connection')
    llmTestOk.value = true
    llmTestMsg.value = r
  } catch (e: any) {
    llmTestOk.value = false
    llmTestMsg.value = `❌ ${String(e ?? '').slice(0, 300)}`
  } finally {
    llmTesting.value = false
  }
}

/** 开机自启开关：以后端返回的真实状态为准，不信前端自己的猜测 */
async function toggleAutostart(kind: 'pet' | 'watchdog') {
  const key = kind === 'pet' ? 'autostart_pet' : 'autostart_watchdog'
  const want = !settings.value[key]
  settingsSaving.value = true
  settingsMsg.value = ''
  try {
    const actual = await invoke<boolean>('set_autostart', { kind, enabled: want })
    settings.value[key] = actual
    settingsBaseline[key] = actual  // 后端已落盘，基准同步跟上
    settingsMsg.value = actual
      ? `✅ 已开启${kind === 'pet' ? '桌宠' : '看门狗'}开机自启`
      : `❌ 已关闭${kind === 'pet' ? '桌宠' : '看门狗'}开机自启`
    setTimeout(() => { settingsMsg.value = '' }, 2500)
  } catch (e) {
    settingsMsg.value = `❌ 操作失败：{String(e).slice(0, 120)}`
    // 失败时以后端真实状态为准
    loadSettings()
  } finally {
    settingsSaving.value = false
  }
}

async function loadMemory() {
  memoryLoading.value = true
  memoryError.value = ''
  try {
    memoryDetail.value = await getMemoryDetail()
  } catch (e: any) {
    memoryError.value = String(e).slice(0, 200)
  }
  memoryLoading.value = false
}

async function toggleMemory() {
  showMemory.value = !showMemory.value
  if (showMemory.value) {
    showChat.value = false  // 互斥：关掉聊天面板
    updateMemoryPosition()
    await loadMemory()
  } else {
    memoryDetail.value = null
  }
  updateWindowRegion()
}

const memoryMsg = ref('')
const showClearConfirm = ref(false)
const showClearMsgConfirm = ref(false)

/** 短暂显示一条操作结果，3 秒后自动消失 */
function showMemoryToast(msg: string) {
  memoryMsg.value = msg
  setTimeout(() => { memoryMsg.value = '' }, 3000)
}

/** 执行记忆操作 、 刷新记忆面板；失败时显示带前缀的错误 */
async function runMemoryAction(action: () => Promise<string>, errorLabel: string) {
  memoryMsg.value = ''
  try {
    showMemoryToast(await action())
    memoryDetail.value = await getMemoryDetail()
  } catch (e) {
    showMemoryToast(`${errorLabel}: ${String(e).slice(0, 100)}`)
  }
}

async function handleDeleteMemory(id: number) {
  await runMemoryAction(() => deleteMemory(id), '删除失败')
}

async function handleClearMemories() {
  if (!showClearConfirm.value) {
    showClearConfirm.value = true
    return
  }
  showClearConfirm.value = false
  await runMemoryAction(() => clearMemories(), '清空失败')
}

async function handleDeleteMessage(id: number) {
  await runMemoryAction(() => deleteMessage(id), '删除失败')
}

async function handleClearMessages() {
  if (!showClearMsgConfirm.value) {
    showClearMsgConfirm.value = true
    return
  }
  showClearMsgConfirm.value = false
  await runMemoryAction(() => clearMessages(), '清空失败')
}

function categoryLabel(c: string) { return c === 'fact' ? '📌 事实' : c === 'preference' ? '💝 偏好' : c }
function formatTime(ts: number) {
  const d = new Date(ts * 1000)
  return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`
}

// ─── 日期格式化 + 节日检测 ───

const WEEKDAY_NAMES = ['星期日', '星期一', '星期二', '星期三', '星期四', '星期五', '星期六']

function formatDateChinese(): string {
  const d = new Date()
  return `${d.getFullYear()}-${d.getMonth() + 1}-${d.getDate()} $${WEEKDAY_NAMES[d.getDay()]}`
}

// 节日查找表（公历固定日期 + 农历节日公历近似日期 2026-2027）
const HOLIDAYS: Record<string, string> = {
  '1-1': '元旦 🎊',
  '2-14': '情人节💕',
  '3-8': '妇女节🌸',
  '4-1': '愚人节🃏',
  '5-1': '劳动节🎉',
  '6-1': '儿童节🎈',
  '10-1': '国庆节🇨🇳',
  '12-24': '平安夜🎄',
  '12-25': '圣诞节🎅',
  // 2026 农历节日（公历对应日期）
  '2-17': '春节 🧧',
  '4-4': '清明节🌿',
  '6-19': '端午节🎋',
  '8-19': '七夕节💕',
  '9-25': '中秋节🥮',
  '10-18': '重阳节🌺',
  '12-22': '冬至 ❄️',
  // 2027 农历节日（公历对应日期）
  '2-6': '春节 🧧',
  '4-5': '清明节🌿',
  '6-8': '端午节🎋',
  '8-8': '七夕节💕',
  '9-14': '中秋节🥮',
  '10-7': '重阳节🌺',
}

function checkHoliday(month: number, day: number, weekday: number): string {
  // 周末标记
  if (weekday === 0) return '周末 ☀️'
  if (weekday === 6) return '周末 🌿'
  // 节日查找
  const key = `${month}-${day}`
  return HOLIDAYS[key] || ''
}

// ─── 右键饼状菜单 ───
const pieMenuVisible = ref(false)
const pieMenuPos = ref({ x: 0, y: 0 })
const pieMenuHovered = ref(-1)
/** 环形菜单中央的设置按钮是否被悬停 */
const pieCenterHovered = ref(false)

const OUTER_R = 115
const INNER_R = 38
const NUM_SECTIONS = 8
const ANGLE_PER_SECTION = (2 * Math.PI) / NUM_SECTIONS

function getSectorPath(i: number): string {
  const sa = -Math.PI / 2 + i * ANGLE_PER_SECTION
  const ea = sa + ANGLE_PER_SECTION
  const x1o = OUTER_R * Math.cos(sa); const y1o = OUTER_R * Math.sin(sa)
  const x2o = OUTER_R * Math.cos(ea); const y2o = OUTER_R * Math.sin(ea)
  const x1i = INNER_R * Math.cos(sa); const y1i = INNER_R * Math.sin(sa)
  const x2i = INNER_R * Math.cos(ea); const y2i = INNER_R * Math.sin(ea)
  return `M ${x1i} ${y1i} L ${x1o} ${y1o} A ${OUTER_R} ${OUTER_R} 0 0 1 ${x2o} ${y2o} L ${x2i} ${y2i} A ${INNER_R} ${INNER_R} 0 0 0 ${x1i} ${y1i} Z`
}
function getLabelPos(i: number): { x: number; y: number } {
  const mid = -Math.PI / 2 + (i + 0.5) * ANGLE_PER_SECTION
  const r = (OUTER_R + INNER_R) / 2
  return { x: r * Math.cos(mid), y: r * Math.sin(mid) }
}
const sectorPaths = Array.from({ length: NUM_SECTIONS }, (_, i) => getSectorPath(i))
const labelPositions = Array.from({ length: NUM_SECTIONS }, (_, i) => getLabelPos(i))

function hidePieMenu() {
  pieMenuVisible.value = false; pieMenuHovered.value = -1; pieCenterHovered.value = false
  updateWindowRegion() // 恢复正常裁剪区域
}
function showPieMenu(x: number, y: number) {
  pieMenuPos.value = { x, y }; pieMenuVisible.value = true
  // 展开为全窗口区域，确保 overlay 全覆盖区域的点击都能被接收
  const dpr = window.devicePixelRatio || 1
  invoke('set_region', { x: 0, y: 0, w: Math.round(window.innerWidth * dpr), h: Math.round(window.innerHeight * dpr) })
}
function onPieItemClick(i: number) { pieMenuItems.value[i]?.action() }

// ─── 第 4/5/6 格：三格网址快捷键 ───

/**
 * 三格的当前状态（名称 / 网址 / 是否已配），顺序 = 饼图上的顺序。
 *
 * 为什么要单独拉一份：设置面板是**点开才加载**的（openSettings → loadSettings），
 * 而饼图一启动就得显示这三个名字。所以启动时用轻量命令 `link_shortcuts` 单独取。
 *
 * 但**点开时不读这里的网址**：动作交给后端按格号去读设置（`open_link_slot`），
 * 免得界面上长期存着一份可能过期的网址副本。
 */
const linkShortcuts = ref<Array<{ name: string; url: string; has_url: boolean }>>([])

async function loadLinkShortcuts() {
  try {
    linkShortcuts.value = await invoke<Array<{ name: string; url: string; has_url: boolean }>>('link_shortcuts')
  } catch { /* 后端没起来：下面有 fallback 名字顶着 */ }
}

/** 第 i 格的显示名（后端还没回话时用默认名） */
function linkLabel(i: number, fallback: string): string {
  const n = linkShortcuts.value[i]?.name
  return (n && n.trim()) || fallback
}

/** 这一格是不是还没配网址 —— 没配就把扇形画暗一点，提示"这里还没填" */
function linkMissing(i: number): boolean {
  const l = linkShortcuts.value[i]
  return !!l && !l.has_url
}

/** 点开某一格：前端只报格号，网址由后端现读设置（并再校验一次协议） */
async function openLinkSlot(i: number) {
  hidePieMenu()
  try {
    await invoke('open_link_slot', { slot: i })
  } catch (e) {
    // 空网址 / 协议不对 / 打不开，都要说出来。这类失败静默掉最难排查
    popBubble(`⚠️ ${String(e).slice(0, 120)}`)
  }
}

/** 饼图一格：三格网址是动态的（名字来自设置），所以整张表是个 computed */
type PieItem = {
  label: string
  icon: string
  /** 有自定义图片就用 <image>，否则用 emoji（icon） */
  image?: string
  /** 没配网址的格子画暗一点，点了会给提示 —— 别让人以为是坏了 */
  off: boolean
  action: () => void
}

const pieMenuItems = computed<PieItem[]>(() => [
  { label: '聊天', icon: '💬', off: false, action: () => { hidePieMenu(); toggleChat() } },
  { label: 'QQ', icon: '🐧', image: '/qq-logo.png', off: false, action: () => { hidePieMenu(); invoke('open_qq') } },
  { label: '动作', icon: '🎭', off: false, action: () => { hidePieMenu(); playRandomMotion(true) } },
  // 这三格的名字和网址都在设置面板里改（原来前两个网址写死在 Rust 里、
  // 而且是作者自己的个人主页地址；中间那格原来是「漂浮」开关，现在改成第三个网址位）
  { label: linkLabel(0, '花园'), icon: '🌸', image: '/dmhy-logo.png', off: linkMissing(0), action: () => openLinkSlot(0) },
  { label: linkLabel(1, '网址'), icon: '🔗', off: linkMissing(1), action: () => openLinkSlot(1) },
  { label: linkLabel(2, 'BGM'), icon: '📋', image: '/bgm-mascot.png', off: linkMissing(2), action: () => openLinkSlot(2) },
  { label: '酷狗', icon: '🎵', off: false, action: () => { hidePieMenu(); invoke('open_kugou_music') } },
  // 第 8 格：饼图本来就是 8 等分，这一格从前一直是空的缺口
  { label: '说明', icon: '📖', off: false, action: () => { hidePieMenu(); openHelp() } },
])

// ─── QQ 邮箱轮询 ───
const emailBubble = ref('')
const showEmailBubble = ref(false)
const emailBubbleStyle = ref<Record<string, string>>({})
const emailRawData = ref('')
let lastEmailKey = ''
let emailQueue: Array<{ uid: string; from: string; subject: string; snippet: string }> = []
let currentEmailUid = ''
let emailCheckTimer: ReturnType<typeof setInterval> | null = null

// ─── 邮件记忆：已关闭（双击跳过）的邮件 UID 持久化到 localStorage ───
const DISMISSED_EMAILS_KEY = 'huohuo_dismissed_emails'

function loadDismissedUids(): Set<string> {
  try {
    const raw = localStorage.getItem(DISMISSED_EMAILS_KEY)
    if (!raw) return new Set()
    const arr = JSON.parse(raw)
    if (!Array.isArray(arr)) return new Set()
    // 只保留最近 200 条，避免无限膨胀
    return new Set(arr.slice(-200).filter((v): v is string => typeof v === 'string'))
  } catch {
    return new Set()
  }
}

function saveDismissedUids(uids: Set<string>) {
  try {
    localStorage.setItem(DISMISSED_EMAILS_KEY, JSON.stringify([...uids]))
  } catch {}
}

// 启动时加载
let dismissedEmailUids = loadDismissedUids()

// ─── 悬浮信息弹窗（天气/日期/节日） ───
const showInfoPopup = ref(false)
const infoPopupStyle = ref<Record<string, string>>({})
type WeatherInfo = {
  city: string
  location: string
  input_location: string
  temperature: string
  feels_like: string
  humidity: string
  wind_speed: string
  weather_desc: string
  weather_icon: string
  /** 未来 3 天预报（和风每日预报接口）。老版本后端没有这个字段，所以是可选的 */
  forecast?: WeatherForecastDay[]
  forecast_text?: string
}

interface WeatherForecastDay {
  label: string
  weather_desc: string
  night_desc: string
  weather_icon: string
  temp_min: string
  temp_max: string
  precip_prob: string
  uv_index: number
}

const WEATHER_LOCATION_KEY = 'huohuo_weather_location'
const LEGACY_WEATHER_CITY_KEY = 'huohuo_weather_city'

const weatherInfo = ref<WeatherInfo | null>(null)
const todayDateStr = ref('')
const todayHoliday = ref('')
const weatherLoading = ref(false)
const weatherError = ref('')
const weatherAddress = ref(localStorage.getItem(WEATHER_LOCATION_KEY) || localStorage.getItem(LEGACY_WEATHER_CITY_KEY) || '')
const showLocationInput = ref(false)
let hoverTimer: ReturnType<typeof setTimeout> | null = null
let infoHideTimer: ReturnType<typeof setTimeout> | null = null
let weatherRequestId = 0
let infoPopupHovered = false
let weatherInputFocused = false

const SPAM_WORDS = ['unsubscribe', 'discount', 'sale', 'offer', 'free', 'win', 'click here',
  '退订', '促销', '优惠', '免费', '中奖', '点击', '广告', '推广']

function isSpam(e: { from: string; subject: string; snippet: string }): boolean {
  const text = `${e.from} ${e.subject} ${e.snippet}`.toLowerCase()
  return SPAM_WORDS.some(w => text.includes(w))
}

function showNextEmail() {
  // 当前正在显示的邮件被用户双击关闭 → 记入"已看"
  if (currentEmailUid) {
    dismissedEmailUids.add(currentEmailUid)
    saveDismissedUids(dismissedEmailUids)
    currentEmailUid = ''
  }

  if (emailQueue.length === 0) {
    showEmailBubble.value = false
    emailBubble.value = ''
    stopSpeech()
    return
  }
  const e = emailQueue.shift()!
  currentEmailUid = e.uid
  // 只显示正文，短邮件全文呈现
  let body = e.snippet || e.subject || '(无内容）'
  // 超过200字截断
  if (body.length > 200) body = body.slice(0, 200) + '...'
  emailBubble.value = `📧 ${e.from}\n\n${body}`
  refreshEmailBubbleStyle()
  showEmailBubble.value = true
}

async function checkEmails() {
  // 没配邮箱就别去连接了—— 每 60 秒空跑一次 IMAP 连接。
  // 既拖慢启动，也会在日志里刷一堆看不懂的失败。
  // 用户在设置里填好之后，下一次轮询（最多 60 秒）就会自动开始收信。
  if (!settings.value.has_email_config) return
  try {
    const emails = await invoke<Array<{ uid: string; from: string; subject: string; snippet: string }>>('check_email')
    if (!emails.length) return
    // 先过滤垃圾邮件，再过滤已关闭过的邮件（記憶機能）
    const useful = emails.filter(e => !isSpam(e) && !dismissedEmailUids.has(e.uid))
    if (!useful.length) return
    const key = useful.map(e => `${e.from}|${e.subject}`).join('||')
    if (key === lastEmailKey) return
    lastEmailKey = key
    emailRawData.value = JSON.stringify(emails, null, 2)
    emailQueue = [...useful.slice(-3)]
    // 主动推送：新邮件同步到 Telegram
    pushEmailToTelegram(useful)
    // 不覆盖正在显示的邮件，等用户双击后再看下一封
    if (!showEmailBubble.value) showNextEmail()
  } catch {}
}

async function copyEmailData() {
  try {
    await navigator.clipboard.writeText(emailRawData.value)
  } catch {}
}

function refreshEmailBubbleStyle() {
  const s = bubbleStyle(-18)
  if (s) emailBubbleStyle.value = s
}

/** 回车发送；Shift+Enter 换行。输入法组词中回车是选字，必须放行不发送 */
function onEnterKey(e: KeyboardEvent) {
  if (e.isComposing || e.keyCode === 229) return
  e.preventDefault()
  sendMessage()
}

/** 清空输入并让输入框收回单行（面板底边随之复原） */
async function clearChatInput() {
  chatInput.value = ''
  await nextTick()
  autoGrowInput()
}

async function sendMessage() {
  const msg = chatInput.value.trim()
  if (!msg || chatLoading.value) return
  // 排队执行：TG 消息可能正在处理中，共用同一套 UI/记忆状态，不能并发
  await enqueuePipeline(() => handleUserMessage(msg))
  await clearChatInput()
}

/**
 * 完整对话管线：模式口令 / 单次模式前缀 / DeepSeek / 情绪表情 / TTS。
 * 返回去掉情绪标签后的最终回复文本。
 *
 * 本地输入框与 Telegram 消息都走这里 —— 行为只有一份实现，
 * 不会出现「手机上能用的口令在电脑上不能用」这类分叉。
 *
 * `forceFileMode`：手机发来的附件走这一条时置真 —— 只对**这一条**用文件模式，
 * 不改变用户当前选的模式（和「文件： xxx」单次前缀同一个思路）。
 */
async function handleUserMessage(msg: string, forceFileMode = false): Promise<string> {
  const text = msg.trim()
  if (!text) return ''

  // ─── 模式口令：精确匹配即切换模式 ───
  // 先剥掉开头的 emoji / 标点：Telegram 的快捷键盘点一下发出来的是
  // 「用文件模式」这种带前缀的文字，用户没必要为此改按钮标签。
  // 仍然以「整串完全相等"才算口令，所以「我不想用文件模式」不会被误判。
  const modeKey = text.replace(/^[^A-Za-z\u4e00-\u9fa5]+/, '').trim()
  const modeWord: Record<string, ChatModeId> = { '聊天模式': 'chat', '问答模式': 'qa', '文件模式': 'file' }
  if (modeWord[modeKey]) {
    const want = modeWord[modeKey]
    const already = chatMode.value === want
    setChatMode(want)
    // 模式没变时 setChatMode 会提前返回、不写 chatReply。
    // 这里必须自己给一句确认，否则返回空串 —— Telegram 那边就什么都收不到。
    return already ? `已经是在${MODE_META[want].label}模式了。` : chatReply.value
  }

  // ─── 「启动」类口令：桌宠已经在运行了，本地答一句就结束 ───
  // 必须拦在这里，绝不能下发给模型：实测在文件模式下发「启动」，
  // 模型会拿着"启动桌宠"这个意图去调 run_command 真去执行 —— 
  // 那是能执行 PowerShell 的工具，结果为了一句口令把桌宠自己搞没了。
  if (WAKE_WORDS.includes(modeKey.toLowerCase())) {
    chatReply.value = '藿藿已经在这里啦～不用叫，我一直在的。'
    return chatReply.value
  }

  // ─── 单次前缀指令：「问答： xxx」「文件： xxx」只用对应模式处理这一条，不改变当前模式 ───
  let effectiveMsg = text
  let effectiveMode: ChatModeId = forceFileMode ? 'file' : chatMode.value
  const prefix = text.match(/^(问答|文件)\s*[:：]\s*(.+)$/)
  if (prefix) {
    effectiveMode = prefix[1] === '问答' ? 'qa' : 'file'
    effectiveMsg = prefix[2]
  }

  // ─── 「新项目」：清空文件模式的项目记忆，从零开始 ───
  // 必须本地处理，不能下发给模型：模型自己删不掉记忆。
  // 让它去理解「忘掉上一个项目」只会得到一句口头应承，记录还在。
  // 放在前缀解析之后，所以「文件: 新项目」这种写法也认。
  const projectKey = effectiveMsg.replace(/^[^A-Za-z\u4e00-\u9fa5]+/, '').trim()
  if (PROJECT_RESET_WORDS.some(w => projectKey.toLowerCase() === w)) {
    try {
      const dropped = await invoke<number>('project_memory_clear')
      chatReply.value = dropped > 0
        ? `🧹 上一个项目的记忆已经清掉了（${dropped} 条），主人想做点什么？`
        : '🧹 项目记忆本来就是空的，直接说要做什么就好。'
      projectMemoryCount.value = 0
    } catch (e) {
      chatReply.value = `❌ 清空项目记忆失败：{String(e).slice(0, 120)}`
    }
    return chatReply.value
  }

  chatLoading.value = true
  const isChat = effectiveMode === 'chat'
  try {
    // 情绪检测与记忆：仅聊天模式生效（问答/文件模式不污染记忆、不动表情）
    let userEmotion: string | null = null
    if (isChat) {
      userEmotion = detectEmotion(effectiveMsg)
      if (userEmotion) applyEmotion(userEmotion)
      addChat('user', effectiveMsg)
    }

    const raw = await invoke<string>('chat_with_deepseek', { msg: effectiveMsg, mode: effectiveMode })
    let parsed: any
    try { parsed = JSON.parse(raw) } catch { parsed = { reply: raw } }
    let reply: string = parsed.reply || raw

    // 解析 [emotion:xxx] 标签（仅聊天模式会产生）
    const emotionMatch = reply.match(/\[emotion:(\w+)\]/i)
    let aiEmotion = ''
    if (emotionMatch) {
      aiEmotion = emotionMatch[1]
      reply = reply.replace(/\[emotion:\w+\]/gi, '').trim()
    }

    chatReply.value = reply

    if (isChat) {
      // 记忆：记录 AI 回复（不含情绪标签）
      addChat('assistant', reply)
      // 触发 Live2D 表情
      if (aiEmotion && !userEmotion) applyEmotion(aiEmotion)
      // 语音朗读（不含标签的纯文本）
      speakWithVolc(reply)
    }
    return reply
  } catch (e: any) {
    // 带上真实失败原因，否则出问题时只看到「信号不好」无从排查
    const reason = String(e ?? '').slice(0, 200)
    const reply = isChat
      ? `呜……藿藿脑子里的信号断掉了……\n（${reason}）`
      : `❌ 请求失败：${reason}`
    chatReply.value = reply
    return reply
  } finally {
    chatLoading.value = false
    // 请求结束（无论成功失败）都要把进度条收掉。
    // 否则她会一直停在「正在读取 xxx.docx"，看起来像卡住了
    fileProgress.value = ''
  }
}

// ─── Telegram 通道 ───

/** 「启动」类口令（与 Rust 的 watch.rs 的 is_start_command 保持一致） */
const WAKE_WORDS = ['启动', '启动桌宠', '开机', '起来', '叫醒', 'start', 'run', 'wake']

/** 「重开项目」类口令：清空文件模式的项目记忆，从零开始。
 *
 *  刻意做成**精确匹配**（不是"包含"）：文件模式下"别忘了把这个文件放桌面"
 *  这种话不能被误判成清空。措辞不全时还有一条兜底 ——
 *  她手上有个 forget_project 工具，可以按你的自然说法真正清空（见 lib.rs）。*/
const PROJECT_RESET_WORDS = [
  // 中文：新项目
  '新项目', '开新项目', '新的项目', '新建项目', '开始新项目', '换个项目', '换项目',
  // 中文：重开 / 重置 / 忘掉
  '重开', '重新开始', '从头开始', '重新来过', '重置', '重置项目', '重开项目',
  '清除记忆', '清空记忆', '清除项目记忆', '清空项目记忆', '删除项目记忆', '忘记项目记忆',
  '忘记之前的项目', '忘掉之前的项目', '忘了之前的项目', '忘掉之前', '忘记之前',
  '把这件事忘记掉', '把这件事忘掉', '忘掉这件事', '忘记这件事', '别再提了',
  // 英文
  'new project', 'restart project', 'start over', 'clear project', 'reset project',
  'forget project', 'forget it',
]

/** 已绑定会话（0 个 = 还没在 Telegram 里给 bot 发过消息） */
const tgChats = ref<Array<{ chat_id: number; title: string }>>([])
const tgRunning = ref(false)
const tgError = ref('')
/** 标题栏 📱 按钮的悬浮提示：把「为什么手机上没反应」直接写在脸上 */
const tgHint = ref('Telegram 通道状态')
/**
 * 心跳超过这个秒数没更新，就按「线路断了」来说。
 * 长轮询一轮 25 秒，健康时 last_poll 每 25~30 秒刷新一次；
 * 状态本身每 60 秒刷一次，所以 90 秒留足了冗余，不会误报。
 */
const TG_STALE_SECS = 90
let tgStatusTimer: ReturnType<typeof setInterval> | null = null
let unlistenTg: (() => void) | null = null

/** 管线串行化：TG 消息与本地输入共用 chatReply/记忆/TTS，并发会互相覆盖 */
let pipelineChain: Promise<unknown> = Promise.resolve()
function enqueuePipeline<T>(fn: () => Promise<T>): Promise<T> {
  const run = pipelineChain.then(fn, fn)
  pipelineChain = run.catch(() => { /* 吞掉，避免链断裂 */ })
  return run
}

/** Telegram 来的消息：当成本地输入处理，结果发回同一个会话 */
async function handleTelegramMessage(payload: { chat_id: number; text: string; from?: string; attachment?: boolean }) {
  // 先回执：后端收到回执就撤掉「桌宠没应答」的兜底诊断。
  // 若这行没跑到（界面没加载 / WebView 崩了），10 秒后手机会收到一条说明，
  // 而不是永远干等 —— 静默失败是最难排查的。
  invoke('telegram_ack', { chatId: payload.chat_id }).catch(() => {})
  await enqueuePipeline(async () => {
    // 附件消息强制用文件模式处理这一条（不改持久模式）：
    // 否则在聊天模式下她没有文件工具，文件收到了也读不了
    const reply = await handleUserMessage(payload.text, payload.attachment === true)
    // 无论如何都不能静默丢掉：手机上「发了没反应」是最难排查的状态
    const text = reply || '（藿藿这边处理完了，但没有内容可以回复）'
    showTgBubble(text)
    try {
      await invoke('telegram_send', { text, chatId: payload.chat_id })
    } catch (e) {
      // 网络抖动，Rust 侧已经重试过了，走到这里说明确实发不出去（多半是梯子断了）
      chatReply.value = `${text}\n\n（⚠️ 回发 Telegram 失败：${String(e).slice(0, 160)}）`
    }
  })
}

/** 用闲时气泡把 TG 消息的回复显示出来（聊天面板没开时也能看到） */
function showTgBubble(text: string) {
  if (showChat.value) return          // 聊天面板已经在显示同一内容。
  popBubble(`📱 ${text.slice(0, 160)}`)
}

/**
 * 在她头顶弹一句话（12 秒后自动消失）。
 *
 * 抽出来是因为"失败必须看得见"的地方不止 Telegram 一处：
 * 饼图里点了一个还没配网址的格子、网址协议不对……这类事如果只写进
 * 聊天面板的 chatReply，聊天面板没开的时候用户根本不知道发生了什么。
 */
function popBubble(text: string) {
  const s = bubbleStyle(-18)
  if (!s) return                      // 模型还没加载好，无处可显示
  idleBubbleStyle.value = s
  idleBubble.value = text
  showIdleBubble.value = true
  if (bubbleTimer) clearTimeout(bubbleTimer)
  bubbleTimer = setTimeout(() => {
    showIdleBubble.value = false
    idleBubble.value = ''
    bubbleTimer = setTimeout(showBubble, 180000 + Math.random() * 60000)
  }, 12000)
}

function refreshTgStatus() {
  invoke<any>('telegram_status').then((s) => {
    tgRunning.value = !!s?.running
    tgChats.value = s?.chats ?? []
    tgError.value = s?.last_error ?? ''
    // 心跳：最后一次**成功**轮询距今多久。
    // 这条通道最坑的失败是静默的 —— 梯子断了的时候，手机上既没有回复也没有报错，
    // 用户在手机那头什么线索都拿不到。所以"多久没心跳"必须能在电脑上看见。
    const lastPoll = Number(s?.last_poll ?? 0)
    const age = lastPoll > 0 ? Math.max(0, Math.floor(Date.now() / 1000) - lastPoll) : -1
    if (!tgRunning.value) {
      tgHint.value = 'Telegram 通道未启动（细节见 %APPDATA%\\desktop-pet\\telegram.log）'
    } else if (tgChats.value.length === 0) {
      tgHint.value = 'Telegram 已连接，但还没有绑定会话：在手机上给 bot 发一条消息即可绑定'
    } else if (age > TG_STALE_SECS) {
      // 心跳比"异常"更值得先说：它直接回答「手机为什么叫不动」
      tgHint.value = `手机通道 ${age} 秒没有心跳 —— 多半是梯子断了，手机发「启动」现在叫不动我（看门狗会一直重试，线路一通就自己恢复）`
    } else if (tgError.value) {
      tgHint.value = `Telegram 异常：${tgError.value}`
    } else if (age >= 0) {
      tgHint.value = `手机通道正常：${age} 秒前刚有心跳，已绑定 ${tgChats.value.length} 个会话（点击刷新）`
    } else {
      tgHint.value = `Telegram 已绑定 ${tgChats.value.length} 个会话，但还没有成功心跳（点击刷新）`
    }
  }).catch(() => { /* 后端未启动 */ })
}

/** 主动推送：新邮件同步到 Telegram（人不在电脑前也能收到） */
function pushEmailToTelegram(list: Array<{ from: string; subject: string; snippet: string }>) {
  const body = list
    .map(e => `📧 ${e.from}\n${(e.snippet || e.subject || '(无内容）').slice(0, 120)}`)
    .join('\n\n')
  invoke('telegram_send', { text: `藿藿帮你看过了，有新邮件：\n\n${body}` })
    .catch(() => { /* 没绑定 / 梯子没开，静默失败，桌宠窗口照常显示 */ })
}


async function speakWithVolc(text: string) {
  // 语音开关默认关（用户要用时再打开），关着时保持安静
  if (!volcEnabled.value) return

  // 豆包语音 （ 火山引擎 TTS
  try {
    await invoke('volc_tts', { text })
    return
  } catch {}

  // 兜底 ： 浏览器 TTS
  const u = new SpeechSynthesisUtterance(text)
  u.lang = 'zh-CN'; u.rate = 1.1; u.pitch = 1.15
  const voices = speechSynthesis.getVoices()
  const zh = voices.find(v => v.lang.startsWith('zh') && v.name.includes('Xiaoxiao'))
    || voices.find(v => v.lang.startsWith('zh'))
  if (zh) u.voice = zh
  speechSynthesis.cancel()
  speechSynthesis.speak(u)
}

async function toggleChat() {
  showChat.value = !showChat.value
  if (showChat.value) {
    chatReply.value = ''
    chatInputGrow = 0
    updateChatPosition()
    await nextTick()
    measureInputBase()
    // 隐藏碎碎念气泡
    showIdleBubble.value = false
    idleBubble.value = ''
    // 打开聊天框时收起天气框（互斥，避免遮挡）
    if (showInfoPopup.value) hideInfoPopup(true)
  } else {
    stopSpeech()
  }
  updateWindowRegion()
}

function stopSpeech() {
  speechSynthesis.cancel()
  invoke('abort_speech').catch(() => {})
}

// ─── Live2D / PIXI ───
let app: PIXI.Application | null = null
let model: Live2DModel | null = null
let idleTimer: ReturnType<typeof setTimeout> | null = null
let gazeTimer: ReturnType<typeof setInterval> | null = null
let cleanupMouse: (() => void) | null = null
let unlistenFocus: (() => void) | null = null
/** 拖放监听（把桌面快捷方式拖进程序位置输入框） */
let unlistenDrop: (() => void) | null = null
let gazeX = 0, gazeY = 0, gazePaused = false
let motionPlaying = false
let motionTimeout: ReturnType<typeof setTimeout> | null = null
let lastInteraction = Date.now()
let isFloating = false
let floatInterval: ReturnType<typeof setInterval> | null = null
let lastMotionName = ''

const MOTIONS = ['haoqi', 'yaotou', 'keshui', 'Scene1', 'zhentou', 'linghun', 'nod', 'breathe', 'happy_jump', 'shy', 'tail_wag', 'idle_stretch', 'surprised_jump', 'hum']
const LOOPING_DURATION: Record<string, number> = { 'zhentou': 6300, 'linghun': 5300 }

const EMOTION_MAP: [RegExp, string][] = [
  [/开心|哈哈|嘻嘻|好耶|太棒|厉害|喜欢|爱/, 'happy'],
  [/生气|烦|讨厌|滚|垃圾|傻|笨|哼/, 'angry'],
  [/难过|哭|伤心|想哭|难受|委屈/, 'sad'],
  [/震惊|卧槽|天哪|不会吧|什么|居然/, 'surprise'],
  [/害怕|恐怖|吓|慌|别|救命/, 'fear'],
  [/害羞|不好意思|脸红|羞/, 'blush'],
  [/微笑|好|嗯|嘿嘿|还行|不错/, 'smile'],
  [/认真|严肃|听着|注意/, 'serious'],
]
let emotionTimer: ReturnType<typeof setTimeout> | null = null

function detectEmotion(text: string): string | null {
  for (const [re, exp] of EMOTION_MAP) { if (re.test(text)) return exp }
  return null
}

function applyEmotion(exp: string) {
  if (!model) return
  try { model.expression(exp) } catch {}
  if (emotionTimer) clearTimeout(emotionTimer)
  emotionTimer = setTimeout(() => {
    try { model?.expression('') } catch {}
    emotionTimer = null
  }, 3000)
}

// ─── Window region ───
const PAD_LEFT = 30, PAD_RIGHT = 30, PAD_BOTTOM = 8

function updateWindowRegion() {
  if (!model || !app) return
  // 饼图打开时保持全窗口可点击，不要裁剪
  if (pieMenuVisible.value) return
  // 拖动任意面板期间同样不裁剪：面板正在移动，裁剪区域追不上它，
  // 指针一旦"掉出"裁剪区，系统就不再往窗口里送事件 —— 拖动会在半路卡死。
  // （拖动开始时已经把整个窗口设为可点击，这里必须让位，否则每 3 帧
  //   就被这条重新裁回去，等于没设。）
  if (panelDragging) return
  const W = app.renderer.width, H = app.renderer.height
  const dpr = window.devicePixelRatio || 1
  const b = model.getBounds()
  let ul = b.x, ut = b.y, ur = b.x + b.width, ub = b.y + b.height

  let hasOverlay = false
  for (const sel of [showChat.value ? '.chat-panel' : '', showMemory.value ? '.memory-panel' : '', showSettings.value ? '.settings-panel' : '', showHelp.value ? '.help-panel' : '', updateDialog.value ? '.update-panel' : '', showIdleBubble.value ? '.idle-bubble' : '', showEmailBubble.value ? '.email-bubble' : '', showInfoPopup.value ? '.info-popup' : '', pieMenuVisible.value ? '.pie-container' : '']) {
    if (!sel) continue
    const el = document.querySelector(sel) as HTMLElement | null
    if (el) { hasOverlay = true; const r = el.getBoundingClientRect(); ul = Math.min(ul, r.left); ut = Math.min(ut, r.top); ur = Math.max(ur, r.right); ub = Math.max(ub, r.bottom) }
  }

  // 聊天/气泡时紧贴(4px)；动作播放时留 80px 防止动画越界被裁；平常完全贴合
  const padTop = hasOverlay ? 4 : (motionPlaying ? 80 : 0)

  const x = Math.max(0, Math.floor((ul - PAD_LEFT) * dpr))
  const y = Math.max(0, Math.floor((ut - padTop) * dpr))
  const w = Math.min(W * dpr - x, Math.ceil((ur - ul + PAD_LEFT + PAD_RIGHT) * dpr))
  const h = Math.min(H * dpr - y, Math.ceil((ub - ut + padTop + PAD_BOTTOM) * dpr))
  invoke('set_region', { x, y, w, h })
}

// ─── 悬浮信息弹窗 ───

function refreshInfoPopupStyle() {
  const s = bubbleStyle(-18)
  if (s) infoPopupStyle.value = s
}

async function fetchWeatherInfo() {
  const address = weatherAddress.value.trim()
  if (!address) {
    weatherInfo.value = null
    weatherError.value = ''
    weatherLoading.value = false
    showLocationInput.value = true
    updateWindowRegion()
    return
  }

  const requestId = ++weatherRequestId
  weatherLoading.value = true
  weatherError.value = ''
  try {
    const raw = await invoke<string>('get_weather_info', {
      cityName: address,
    })
    if (requestId !== weatherRequestId) return
    weatherInfo.value = JSON.parse(raw)
  } catch (e: any) {
    if (requestId !== weatherRequestId) return
    weatherError.value = String(e || '天气获取失败').slice(0, 180)
    weatherInfo.value = null
    showLocationInput.value = true
  } finally {
    if (requestId === weatherRequestId) {
      weatherLoading.value = false
      updateWindowRegion()
    }
  }
}

function showInfoPopupNow() {
  if (showInfoPopup.value) return
  // 聊天框开着时天气框不出现（避免相互遮挡）
  if (showChat.value) return
  const now = new Date()
  todayDateStr.value = formatDateChinese()
  todayHoliday.value = checkHoliday(now.getMonth() + 1, now.getDate(), now.getDay())
  refreshInfoPopupStyle()
  showLocationInput.value = !weatherAddress.value.trim()
  if (weatherAddress.value.trim()) {
    fetchWeatherInfo()
  } else {
    weatherInfo.value = null
    weatherError.value = ''
    weatherLoading.value = false
  }
  showInfoPopup.value = true
  updateWindowRegion()
}

function clearInfoHideTimer() {
  if (infoHideTimer) { clearTimeout(infoHideTimer); infoHideTimer = null }
}

function hideInfoPopup(force = false) {
  if (!force && (infoPopupHovered || weatherInputFocused)) return
  if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null }
  clearInfoHideTimer()
  showInfoPopup.value = false
  weatherInfo.value = null
  weatherError.value = ''
  infoPopupHovered = false
  weatherInputFocused = false
  updateWindowRegion()
}

function cancelInfoHide() {
  clearInfoHideTimer()
}

function scheduleInfoHide(delay = 700) {
  clearInfoHideTimer()
  infoHideTimer = setTimeout(() => {
    if (!infoPopupHovered && !weatherInputFocused) {
      hideInfoPopup(true)
    }
  }, delay)
}

function onInfoPopupEnter() {
  infoPopupHovered = true
  cancelInfoHide()
}

function onInfoPopupLeave() {
  infoPopupHovered = false
  scheduleInfoHide(0)
}

function onWeatherInputFocus() {
  weatherInputFocused = true
  cancelInfoHide()
}

function onWeatherInputBlur() {
  weatherInputFocused = false
  saveWeatherAddress()
  if (!infoPopupHovered) scheduleInfoHide(0)
}

function saveWeatherAddress() {
  const v = weatherAddress.value.trim()
  weatherAddress.value = v
  if (v) {
    // 只在非空时同步进设置。这里原来**无条件**同步，而输入框失焦也会走到这儿 —— 
    // 结果是「点一下地址框再点到别处"就把已保存的城市清成了空串」。
    // 清空输入框只是让弹窗回到"待输入"状态，不该动设置里的默认地点。
    syncWeatherLocationToSettings(v)
    localStorage.setItem(WEATHER_LOCATION_KEY, v)
    localStorage.setItem(LEGACY_WEATHER_CITY_KEY, v)
    showLocationInput.value = false
    fetchWeatherInfo()
  } else {
    localStorage.removeItem(WEATHER_LOCATION_KEY)
    localStorage.removeItem(LEGACY_WEATHER_CITY_KEY)
    weatherInfo.value = null
    weatherError.value = ''
    weatherLoading.value = false
    showLocationInput.value = true
    updateWindowRegion()
  }
}

/** 切换豆包语音开关：立即落盘（原来只存在前端 ref 里，重启就忘）了 
 *  聊天框那个按钮已按要求移除，现在只有设置面板里能开关。 */
function setVolcEnabled(on: boolean) {
  volcEnabled.value = on
  settings.value.volc_enabled = on
  invoke('set_volc_enabled', { enabled: on }).catch(() => { /* 后端未就绪时不打紧 */ })
  // 开关打开了但凭据没配 — 说清楚会退化成系统语音，并指路
  if (on && !settings.value.has_volc_credentials) {
    chatReply.value =
      '⚠️ 还没配豆包语音凭据，会先用系统语音顶上。\n右键呼出环形菜单 → ⚙️ 设置 → 填「豆包 App ID」和「Access Token」即可'
  }
}

/** 把地点写进后端设置（失败就算了，不影响弹窗本身能用） */
function syncWeatherLocationToSettings(v: string) {
  settings.value.weather_location = v
  invoke('save_settings', { weatherLocation: v }).catch(() => {})
}

function editWeatherAddress() {
  showLocationInput.value = true
  weatherError.value = ''
  updateWindowRegion()
}

function cancelWeatherAddressEdit() {
  showLocationInput.value = !weatherAddress.value.trim() || !weatherInfo.value
}

// ─── Mouse ───
function setupMouseInteraction(): () => void {
  const el = canvasRef.value
  if (!el) return () => {}

  let down = false, lx = 0, ly = 0, sx = 0, sy = 0, moved = 0, dragged = false
  let clicks = 0, clickTimer: ReturnType<typeof setTimeout> | null = null

  const resetClicks = () => { clicks = 0; if (clickTimer) { clearTimeout(clickTimer); clickTimer = null } }

  const onDown = (e: PointerEvent) => {
    if (e.button !== 0) return
    lastInteraction = Date.now(); stopFloat()
    down = true; lx = e.clientX; ly = e.clientY; sx = e.clientX; sy = e.clientY
    moved = 0; dragged = false
  }

  const onMove = (e: PointerEvent) => {
    if (!down) return
    moved += Math.abs(e.clientX - lx) + Math.abs(e.clientY - ly)
    lx = e.clientX; ly = e.clientY
    if (!dragged && moved > 25) { dragged = true; invoke('drag_window') }
  }

  const onUp = async (_e: PointerEvent) => {
    if (!down) return
    const wasClick = moved < 25
    down = false
    if (!wasClick) { updateWindowRegion(); return }

    clicks++
    if (clickTimer) clearTimeout(clickTimer)
    clickTimer = setTimeout(async () => {
      const n = clicks; resetClicks()
      if (n >= 4) { invoke('exit_app') }
      else if (n >= 2) toggleChat()
      else {
        if (model) {
          const b = model.getBounds()
          const onHead = sx >= b.x && sx <= b.x + b.width && sy >= b.y && sy <= b.y + b.height * 0.3
          if (onHead) await playMotion('linghun')
          else await playRandomMotion(true, 'linghun')
        }
      }
    }, 1000)
    updateWindowRegion()
  }

  const onContextMenu = (e: MouseEvent) => {
    e.preventDefault()
    lastInteraction = Date.now(); stopFloat()
    const cx = Math.min(Math.max(e.clientX, OUTER_R + 10), window.innerWidth - OUTER_R - 10)
    const cy = Math.min(Math.max(e.clientY, OUTER_R + 10), window.innerHeight - OUTER_R - 10)
    showPieMenu(cx, cy)
  }

  // 滚轮缩放：与设置面板里的「桌宠大小」共用同一个入口 applyPetScale，
  // 所以限位范围一致（原来这里自己夹到 0.005~0.08，和设置里的 0.02~0.10 不一样，
  // 滚出来的值一旦落盘会被后端夹回去，大小会莫名跳一下）。
  const onWheel = (e: WheelEvent) => {
    if (!model) return
    const b = model.getBounds()
    const inside = e.clientX >= b.x && e.clientX <= b.x + b.width && e.clientY >= b.y && e.clientY <= b.y + b.height
    if (!inside) return
    e.preventDefault()
    lastInteraction = Date.now(); stopFloat()
    const step = e.deltaY < 0 ? 1.08 : 0.925
    applyPetScale(model.scale.x * step)
    // 滚轮事件触发得很密，等停手再落盘（否则一次滚动会写十几次设置文件）
    persistPetScaleSoon()
  }

  el.addEventListener('pointerdown', onDown, true)
  el.addEventListener('pointermove', onMove, true)
  el.addEventListener('pointerup', onUp, true)
  el.addEventListener('contextmenu', onContextMenu, true)
  el.addEventListener('wheel', onWheel, { passive: false })

  // 悬浮检测：鼠标在宠物上停留1秒显示信息弹窗
  const onMouseEnter = () => {
    if (infoHideTimer) { clearTimeout(infoHideTimer); infoHideTimer = null }
    if (!hoverTimer) {
      hoverTimer = setTimeout(showInfoPopupNow, 1000)
    }
  }
  const onMouseLeave = () => {
    if (hoverTimer) { clearTimeout(hoverTimer); hoverTimer = null }
    // 鼠标一离开宠物立即关闭；setTimeout(0) 只是给同帧内鼠标进入弹窗的
    // 想点输入框时）留出取消机会，视觉上就是「一挪开就关闭」
    if (showInfoPopup.value) {
      scheduleInfoHide(0)
    }
  }
  el.addEventListener('mouseenter', onMouseEnter)
  el.addEventListener('mouseleave', onMouseLeave)

  return () => {
    resetClicks()
    el.removeEventListener('wheel', onWheel)
    el.removeEventListener('pointerdown', onDown, true)
    el.removeEventListener('pointermove', onMove, true)
    el.removeEventListener('pointerup', onUp, true)
    el.removeEventListener('contextmenu', onContextMenu, true)
    el.removeEventListener('mouseenter', onMouseEnter)
    el.removeEventListener('mouseleave', onMouseLeave)
  }
}

// ─── Gaze ───
function setupGazeTracking() {
  if (gazeTimer) clearInterval(gazeTimer)
  gazeTimer = setInterval(async () => {
    if (!model || gazePaused) return
    try {
      const [mx, my] = await invoke<[number, number]>('get_cursor_pos')
      const w = getCurrentWindow()
      const pos = await w.outerPosition(); const size = await w.outerSize()
      const cx = pos.x + size.width / 2; const cy = pos.y + size.height * 0.15
      const speed = 0.32
      gazeX += (Math.max(-1, Math.min(1, (mx - cx) / 300)) - gazeX) * speed
      gazeY += (Math.max(-1, Math.min(1, (my - cy) / 300)) - gazeY) * speed
      const mm = (model as any).internalModel?.coreModel
      if (mm) {
        try { mm.setParameterValueById('ParamAngleX', gazeX * 30) } catch {}
        try { mm.setParameterValueById('ParamAngleY', -gazeY * 30) } catch {}
        try { mm.setParameterValueById('ParamEyeBallX', gazeX) } catch {}
        try { mm.setParameterValueById('ParamEyeBallY', -gazeY) } catch {}
        try { mm.setParameterValueById('ParamBodyAngleX', gazeX * 10) } catch {}
      }
    } catch {}
  }, 20)
}

// ─── Motion ───
function resetHeadParams() {
  const mm = (model as any)?.internalModel?.coreModel
  if (!mm) return
  for (const [id, val] of [
    ['ParamAngleX',0],['ParamAngleY',0],['ParamAngleZ',0],['ParamBodyAngleX',0],['ParamBodyAngleY',0],['ParamBodyAngleZ',0],
    ['ParamEyeBallX',0],['ParamEyeBallY',0],['ParamEyeLOpen',1],['ParamEyeROpen',1],['ParamEyeLSmile',0],['ParamEyeRSmile',0],
    ['ParamBrowLY',0],['ParamBrowLAngle',0],['ParamBrowRY',0],['ParamBrowRAngle',0],['ParamMouthForm',0],['ParamMouthOpenY',0],
    ['Param99',0],['Param119',0],['Param120',0],['Param121',0],['Param122',0],['Param123',0],['Param124',0],['Param125',0],['Param126',0],
    ['Param110',0],['Param1',0],['Param2',0],['Param3',0],['Param4',0],['Param5',0],['Param6',0],
  ] as [string, number][]) {
    try { mm.setParameterValueById(id, val) } catch {}
  }
  gazeX = 0; gazeY = 0
}

function finishMotion() {
  if (motionTimeout) { clearTimeout(motionTimeout); motionTimeout = null }
  try { (model as any)?.internalModel?.motionManager?.stopAllMotions() } catch {}
  requestAnimationFrame(() => {
    resetHeadParams()
    requestAnimationFrame(() => { gazePaused = false; motionPlaying = false; scheduleIdleMotion() })
  })
}

/** 打断当前动作：停循环计时器、恢复视线、停掉所有动作，等 80ms 让 Live2D 稳定 */
async function stopCurrentMotion() {
  if (motionTimeout) { clearTimeout(motionTimeout); motionTimeout = null }
  gazePaused = false
  try { (model as any)?.internalModel?.motionManager?.stopAllMotions() } catch {}
  await new Promise(r => setTimeout(r, 80))
}

/** 启动动作前半段；循环动作会接管并设置结束计时器。返回 true 表示循环动作已接管，调用方直接返回 */
function beginMotion(name: string): boolean {
  motionPlaying = true
  if (idleTimer) { clearTimeout(idleTimer); idleTimer = null }
  lastMotionName = name
  const dur = LOOPING_DURATION[name]
  if (dur) {
    if (name === 'zhentou') gazePaused = true
    try { (model as any)?.internalModel?.motionManager?.startMotion(name, 0, 2) } catch {}
    motionTimeout = setTimeout(finishMotion, dur)
    return true
  }
  return false
}

/** 一次性动作后半段：播放并调度下一次待机动作 */
async function playOneShotMotion(name: string) {
  const mm = (model as any)?.internalModel?.motionManager
  try { const ok = await model!.motion(name, 0, 2); if (!ok && mm) await mm.startMotion(name, 0, 2) } catch {}
  motionPlaying = false
  scheduleIdleMotion()
}

async function playMotion(name: string) {
  if (!model) return
  if (motionPlaying) await stopCurrentMotion()
  if (beginMotion(name)) return
  await playOneShotMotion(name)
}

async function playRandomMotion(force = false, exclude?: string) {
  if (!model || (motionPlaying && !force)) return
  if (force && motionPlaying && lastMotionName in LOOPING_DURATION) return
  if (force && motionPlaying) await stopCurrentMotion()
  const pool = MOTIONS.filter(m => m !== exclude)
  const name = pool[Math.floor(Math.random() * pool.length)]
  if (beginMotion(name)) return
  await playOneShotMotion(name)
}

// ─── Float ───
let fsx = 0, fsy = 0, ftx = 0, fty = 0, fnp = 0

/** 默认闲置 5 分钟开始漂浮 */
const FLOAT_IDLE_SECONDS_DEFAULT = 300

/**
 * 闲置多久开始漂浮。
 *
 * **可以从 `window.__petFloatIdleSeconds` 覆盖** —— 这是给验收脚本留的口子：
 * 真等 5 分钟没法测，而"面板开着到底飘不飘"只有让窗口真的动/不动才算验证过
 * （见 `scripts/float-check.mjs`）。
 */
function floatIdleSeconds(): number {
  const override = Number((window as unknown as { __petFloatIdleSeconds?: number }).__petFloatIdleSeconds)
  return Number.isFinite(override) && override > 0 ? override : FLOAT_IDLE_SECONDS_DEFAULT
}

/**
 * 这些面板开着的时候**禁止漂浮**。
 *
 * 面板是**跟着桌宠定位**的（以宠物头顶为锚点算坐标），桌宠一飘，面板就被拖着满屏走 ——
 * 用户正在里面填设置，鼠标还没跟上它。原来的规则只看"有没有交互"，
 * 而"开着面板不碰鼠标"再常见不过，5 分钟一到就飘走了（主人反馈）。
 */
function floatBlocked(): boolean {
  return showSettings.value
}

async function startFloat() {
  if (isFloating || !model || floatBlocked()) return
  isFloating = true
  const sw = window.screen.width, sh = window.screen.height, dpr = window.devicePixelRatio || 1
  const b = model.getBounds(); const mw = b.width; const mh = b.height; const pad = 30
  const win = getCurrentWindow()
  try { const p = await win.outerPosition(); fsx = p.x / dpr + model.x - mw / 2; fsy = p.y / dpr + model.y - mh }
  catch { fsx = 100; fsy = 100 }
  ftx = Math.random() * (sw - mw - pad * 2) + pad
  fty = Math.random() * (sh - mh - pad * 2) + pad
  fnp = performance.now() + 3000 + Math.random() * 5000
  let lt = performance.now()

  floatInterval = setInterval(() => {
    if (!isFloating || motionPlaying) return
    const now = performance.now()
    if (now >= fnp) { ftx = Math.random() * (sw - mw - pad * 2) + pad; fty = Math.random() * (sh - mh - pad * 2) + pad; fnp = now + 3000 + Math.random() * 5000 }
    const dt = Math.min((now - lt) / 1000, 0.1); lt = now
    const dx = ftx - fsx, dy = fty - fsy; const dist = Math.sqrt(dx * dx + dy * dy)
    if (dist > 1) { fsx += (dx / dist) * Math.min(53 * dt, dist); fsy += (dy / dist) * Math.min(53 * dt, dist) }
    invoke('set_window_pos', { x: Math.round((fsx - model!.x + mw / 2) * dpr), y: Math.round((fsy - model!.y + mh) * dpr) }).catch(() => {})
  }, 16)
}

function stopFloat() { isFloating = false; if (floatInterval) { clearInterval(floatInterval); floatInterval = null } }

function scheduleIdleMotion() {
  if (idleTimer) clearTimeout(idleTimer)
  idleTimer = setTimeout(async () => {
    while (motionPlaying) await new Promise(r => setTimeout(r, 200))
    await playRandomMotion()
  }, 8000 + Math.random() * 10000)
}

function onResize() {
  if (!app) return
  app.renderer.resize(window.innerWidth, window.innerHeight)
  if (model) { model.x = window.innerWidth / 2; model.y = window.innerHeight - 30 }
  if (showMemory.value) updateMemoryPosition()
  if (showHelp.value) updateHelpPosition()
  if (showChat.value) {
    updateChatPosition()
    autoGrowInput()   // 窗口尺寸变了，输入框可用高度也要重算
  }
  updateWindowRegion()
}

// ─── Init / Cleanup ───
onMounted(async () => {
  // ─── Telegram 通道监听 ───
  // 刻意放在最前面：下面 Live2D 加载失败会 return，
  // 若注册在后面，模型一旦坏，Telegram 就整个哑掉。
  unlistenTg = await listen<{ chat_id: number; text: string; from?: string }>('tg-message', (e) => {
    // 不能让它变成未捕获的 Promise 拒绝（WebView 控制台会刷红）
    handleTelegramMessage(e.payload).catch(() => {})
  })
  refreshTgStatus()
  tgStatusTimer = setInterval(refreshTgStatus, 60000)

  // 饼图那三格网址快捷键的名字：设置面板是点开才加载的，所以这里单独拉一次。
  // 放在 Live2D 加载之前 —— 模型加载失败会 return，饼图的名字不该跟着一起没有
  loadLinkShortcuts()

  // 文件模式的进度：后端每推进一步就发一条，这里显示出来。
  // 纯展示，出错也不该影响主流程，所以不往外抛。
  unlistenFileProgress = await listen<{ text: string }>('pet-file-progress', (e) => {
    fileProgress.value = String(e.payload?.text || '')
  })

  // 更新包的下载进度（几 MB 的包，不给动静的话用户会以为卡死）。
  // 事件里带的是结构化数字，进度条、速度才画得出来 ——
  // 从"正在下载… 42%"里正则抠数字是荒唐的。
  unlistenUpdate = await listen<any>('pet-update-progress', (e) => {
    const p = e.payload || {}
    updatePhase.value = String(p.phase || '')
    updatePercent.value = Number(p.percent || 0)
    const got = Number(p.got || 0)
    // 速度：用相邻两次事件的字节差 / 时间差算，后端没必要为此存状态
    const now = Date.now()
    if (updateLastGot >= 0 && now > updateLastTs && got > updateLastGot) {
      const bps = (got - updateLastGot) / ((now - updateLastTs) / 1000)
      updateSpeed.value = bps > 0 ? `${formatBytes(bps)}/s` : ''
    }
    updateLastGot = got
    updateLastTs = now
    updateGot.value = got
    if (p.total) updateTotal.value = Number(p.total)
  })
  // 当前版本/构建时间：面板一打开就要显示，所以启动时先取一次
  loadUpdateInfo()
  // 全局快捷键：后端（hotkeys.rs）注册好后按下会推这个事件，动作在这里执行
  unlistenHotkey = await listen<{ action: string }>('pet-hotkey', (e) => {
    const action = e.payload?.action
    if (action === 'chat') toggleChat()
    else if (action === 'action') playRandomMotion(true)
    else if (action === 'toggle') togglePetVisible()
    // 第二次双击快捷方式：已经运行的那只会收到这个（见 instance::request_summon）。
    // 桌宠可能被上面那个快捷键藏起来了，这样"再双击一次图标"也能把它叫回来。
    else if (action === 'summon') {
      getCurrentWindow().show().then(() => getCurrentWindow().setFocus()).catch(() => {})
      popBubble('藿藿在这儿呢～')
    }
  })
  refreshHotkeyStatus()
  // 启动后静默检查一次更新（像正经软件那样）。延迟 8 秒：先把模型和界面加载好，
  // 别和启动抢网络；没配置更新地址时后端会直接返回错误，这里静默吞掉。
  // 开关读的是**后端**给的值（updateInfo.auto_check），不是前端的默认值 ——
  // 设置面板是点开才加载的，用前端默认值会让"关掉自动检查"形同虚设。
  updateAutoTimer = setTimeout(() => {
    if (updateInfo.value.auto_check && updateInfo.value.update_url) checkUpdate(true)
  }, 8000)

  if (!canvasRef.value) return
  const el = canvasRef.value
  const W = window.innerWidth, H = window.innerHeight

  app = new PIXI.Application({ view: el, width: W, height: H, backgroundAlpha: 0, antialias: true, resolution: window.devicePixelRatio || 1, autoDensity: true })
  Live2DModel.registerTicker(PIXI.Ticker)

  try { model = await Live2DModel.from('/model/character/huohuo.model3.json') }
  catch (err: any) { return }

  model.anchor.set(0.5, 1); model.scale.set(settings.value.pet_scale); model.x = W / 2; model.y = H - 30
  app.stage.addChild(model)

  // 拉一次设置：桌宠大小、天气地点都按用户保存的值来（拉不到就用默认值）
  await loadSettings()
  try {
    model.scale.set(settings.value.pet_scale)
    if (settings.value.weather_location) weatherAddress.value = settings.value.weather_location
    // 语音开关是持久化的，启动时跟着设置走
    volcEnabled.value = settings.value.volc_enabled
  } catch { /* 模型已销毁等极端情况，忽略 */ }

  // 第一次运行又还没配密钥， 直接把设置面板打开。
  // 程序不再内置密钥了，不主动引导的话用户只会看到「请求失败"而找不到原因」。
  // 只提示一次，之后不反复打扰。
  if (!settings.value.has_api_key) {
    let nagged = false
    try { nagged = localStorage.getItem('huohuo_api_key_nagged') === '1' } catch { /* 忽略 */ }
    if (!nagged) {
      try { localStorage.setItem('huohuo_api_key_nagged', '1') } catch { /* 忽略 */ }
      openSettings()
    }
  }

  updateWindowRegion()
  try { await invoke('enable_cursor') } catch {}

  bubbleTimer = setTimeout(showBubble, 180000)

  // 闲置到点就漂浮；但**面板开着时禁止**（面板跟着桌宠定位，一飘就被拖着走）
  setInterval(() => {
    if ((Date.now() - lastInteraction) / 1000 >= floatIdleSeconds() && !isFloating && !floatBlocked()) startFloat()
  }, 5000)

  let tf = 0
  app.ticker.add(() => { tf++; if (tf % 3 === 0) updateWindowRegion() })

  cleanupMouse = setupMouseInteraction()
  setupGazeTracking()
  scheduleIdleMotion()
  window.addEventListener('resize', onResize)
  speechSynthesis.getVoices()
  speechSynthesis.onvoiceschanged = () => { speechSynthesis.getVoices() }

  // 窗口失焦时自动关闭饼图（用户点击了桌面上其他位置）
  const appWindow = getCurrentWindow()
  unlistenFocus = await appWindow.onFocusChanged((event) => {
    if (!event.payload && pieMenuVisible.value) {
      hidePieMenu()
    }
  })

  // 拖放：把桌面上的快捷方式拖进「程序位置」输入框 → 自动解析成它指向的 .exe。
  // 用的是 Tauri 的拖放事件而不是 HTML5 的 drop：窗口开着 dragDropEnabled（默认），
  // 文件拖进来会被 Tauri 接管，网页里的 drop 事件根本不会触发。
  try {
    unlistenDrop = await getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload as any
      if (p?.type !== 'drop') return
      handleFileDrop(p.paths || [], p.position)
    })
  } catch { /* 拿不到监听也不影响手填 */ }

  // 收件邮箱轮询（每60秒）。没配置时 checkEmails 会自己早退
  checkEmails()
  emailCheckTimer = setInterval(checkEmails, 60000)

})

onUnmounted(async () => {
  // 应用关闭时，保存当前正在显示的邮件 UID（防止遗漏）
  if (currentEmailUid) {
    dismissedEmailUids.add(currentEmailUid)
    saveDismissedUids(dismissedEmailUids)
    currentEmailUid = ''
  }
  cleanupMouse?.()
  unlistenFocus?.()
  unlistenDrop?.()
  unlistenTg?.()
  unlistenFileProgress?.()
  unlistenUpdate?.()
  unlistenHotkey?.()
  if (tgStatusTimer) clearInterval(tgStatusTimer)
  if (petScaleSaveTimer) clearTimeout(petScaleSaveTimer)
  window.removeEventListener('resize', onResize)
  if (idleTimer) clearTimeout(idleTimer)
  if (gazeTimer) clearInterval(gazeTimer)
  if (motionTimeout) clearTimeout(motionTimeout)
  if (floatInterval) clearInterval(floatInterval)
  if (bubbleTimer) clearTimeout(bubbleTimer)
  if (emailCheckTimer) clearInterval(emailCheckTimer)
  if (updateAutoTimer) clearTimeout(updateAutoTimer)
  if (hoverTimer) clearTimeout(hoverTimer)
  if (infoHideTimer) clearTimeout(infoHideTimer)
  speechSynthesis.cancel()
  app?.destroy(true, { children: true })
})
</script>

<template>
  <div class="app">
    <canvas ref="canvasRef"></canvas>

    <div v-if="showIdleBubble" class="idle-bubble" :style="idleBubbleStyle">{{ idleBubble }}</div>

    <div v-if="showEmailBubble" class="email-bubble" :style="emailBubbleStyle" @dblclick="showNextEmail()">
      {{ emailBubble }}
      <button class="email-copy-btn" @click="copyEmailData" title="复制原始数据">📋</button>
    </div>

    <div v-if="showChat" class="chat-panel" :style="chatStyle">
      <div
        class="chat-header panel-drag-handle"
        title="按住这里可以把聊天面板拖到任意位置；双击恢复跟随桌宠"
        @pointerdown="onChatDragStart"
        @dblclick="resetChatPos"
      >
        <span>💬 跟藿藿聊天</span>
        <div class="chat-header-actions">
          <button class="chat-toggle-btn memory-btn" @click="toggleMemory()" title="查看藿藿的记忆">记忆</button>
          <button
            class="chat-toggle-btn tg-indicator"
            :class="{ off: !tgRunning || tgChats.length === 0 }"
            :title="tgHint"
            @click="refreshTgStatus()"
          >📱</button>
          <button class="chat-close" @click="showChat = false; stopSpeech(); updateWindowRegion()">✕</button>
        </div>
      </div>
      <div class="mode-bar">
        <button
          v-for="m in MODE_ORDER"
          :key="m"
          class="mode-btn"
          :class="{ active: chatMode === m }"
          @click="setChatMode(m)"
        >
          {{ MODE_META[m].label }}
        </button>
      </div>
      <div class="mode-hint">{{ MODE_META[chatMode].hint }}</div>
      <div v-if="chatReply" class="chat-reply">{{ chatReply }}</div>
      <!-- 文件模式进度：她正在读哪个文件、写到哪一步。 
           长活儿要跑十几秒到几十秒，没有这个就只能干等 -->
      <div v-if="fileProgress" class="chat-progress">
        <span class="chat-progress-dot"></span>{{ fileProgress }}
      </div>
      <div class="chat-input-row">
        <textarea
          ref="chatInputRef"
          v-model="chatInput"
          class="chat-input"
          rows="1"
          :placeholder="MODE_META[chatMode].placeholder"
          :disabled="chatLoading"
          @input="autoGrowInput"
          @keydown.enter.exact="onEnterKey"
        ></textarea>
        <button @click="sendMessage" :disabled="chatLoading">{{ chatLoading ? '...' : '发送' }}</button>
      </div>
    </div>

    <!-- 记忆查看面板 -->
    <div v-if="showMemory" class="memory-panel" :style="memoryStyle">
      <div
        class="chat-header panel-drag-handle"
        title="按住这里可以把记忆面板拖到任意位置；双击恢复跟随桌宠"
        @pointerdown="onMemoryDragStart"
        @dblclick="resetMemoryPos"
      >
        <span>🧠 藿藿的记忆</span>
        <button class="chat-close" @click="toggleMemory()">✕</button>
      </div>

      <div class="memory-body">
        <!-- 用户名字 -->
        <div v-if="memoryDetail?.user_name" class="memory-section">
          <div class="memory-section-title">👤 用户</div>
          <div class="memory-item fact">{{ memoryDetail.user_name }}</div>
        </div>

        <!-- 记忆条目 -->
        <div v-if="memoryDetail?.memories.length" class="memory-section">
          <div class="memory-section-title">
            📝 记忆 ({{ memoryDetail.memories.length }})
          </div>
          <div
            v-for="m in memoryDetail.memories"
            :key="m.id"
            :class="['memory-item', m.category]"
          >
            <span class="memory-cat">{{ categoryLabel(m.category) }}</span>
            <span class="memory-content">{{ m.content }}</span>
            <button class="memory-del" @click="handleDeleteMemory(m.id)" title="删除">🗑</button>
          </div>
        </div>
        <div v-else-if="!memoryLoading" class="memory-empty">
          🦊 藿藿还没记住什么呢……多跟她聊聊吧！
        </div>

        <!-- 最近聊天 -->
        <div v-if="memoryDetail?.recent_messages.length" class="memory-section">
          <div class="memory-section-title">💬 最近聊天</div>
          <div
            v-for="m in memoryDetail.recent_messages"
            :key="m.id"
            :class="['memory-msg', m.role]"
          >
            <span class="memory-role">{{ m.role === 'user' ? '👤' : '🦊' }}</span>
            <span class="memory-msg-text">{{ m.content }}</span>
            <span class="memory-time">{{ formatTime(m.time) }}</span>
            <button class="memory-del" @click="handleDeleteMessage(m.id)" title="删除">🗑</button>
          </div>
        </div>

        <div v-if="memoryLoading" class="memory-loading">🔍 正在读取记忆……</div>
        <div v-if="memoryError" class="memory-error">{{ memoryError }}</div>
      </div>

      <!-- 底部操作 -->
      <div class="memory-footer">
        <div v-if="memoryMsg" class="memory-toast">{{ memoryMsg }}</div>
        <button class="memory-refresh" @click="loadMemory()" :disabled="memoryLoading">🔄 刷新</button>
        <button class="memory-clear" @click="handleClearMessages()" :disabled="memoryLoading">
          {{ showClearMsgConfirm ? '⚠️ 确认清空聊天？' : '🗑 清空聊天' }}
        </button>
        <button class="memory-clear" @click="handleClearMemories()" :disabled="memoryLoading">
          {{ showClearConfirm ? '⚠️ 确认清空记忆？' : '🗑 清空记忆' }}
        </button>
      </div>
    </div>

    <!-- 悬浮信息弹窗（天气/日期/节日） -->
    <div
      v-if="showInfoPopup"
      class="info-popup"
      :style="infoPopupStyle"
      @mouseenter="onInfoPopupEnter()"
      @mouseleave="onInfoPopupLeave()"
      @pointerenter="onInfoPopupEnter()"
      @pointerleave="onInfoPopupLeave()"
    >
      <div class="info-date">{{ todayDateStr }}</div>
      <div v-if="todayHoliday" class="info-holiday">{{ todayHoliday }}</div>
      <div v-if="weatherLoading" class="info-weather-loading">⏳ 天气加载中..</div>
      <div v-else-if="weatherInfo" class="info-weather">
        <span class="info-weather-icon">{{ weatherInfo.weather_icon }}</span>
        <span class="info-temp">{{ weatherInfo.temperature }}°C</span>
        <span class="info-desc">{{ weatherInfo.weather_desc }}</span>
        <div class="info-details">
          体感 {{ weatherInfo.feels_like }}°C · 湿度 {{ weatherInfo.humidity }}% · 风速 {{ weatherInfo.wind_speed }} km/h
        </div>
        <!-- 未来 3 天预报（拿不到时不显示这一块，实时天气照常） -->
        <div v-if="weatherInfo.forecast?.length" class="info-forecast">
          <div v-for="f in weatherInfo.forecast" :key="f.label" class="info-forecast-day">
            <span class="info-forecast-label">{{ f.label }}</span>
            <span class="info-forecast-icon">{{ f.weather_icon }}</span>
            <span class="info-forecast-temp">{{ f.temp_min }}~{{ f.temp_max }}°</span>
            <span class="info-forecast-desc">{{ f.weather_desc }}</span>
          </div>
        </div>
      </div>
      <div v-else-if="weatherError" class="info-weather-error">{{ weatherError }}</div>
      <div v-else class="info-weather-empty">📍 输入地址查看天气</div>

      <div class="info-location-row">
        <input
          v-if="showLocationInput || !weatherAddress.trim() || weatherError"
          v-model="weatherAddress"
          class="info-location-input"
          placeholder="输入地址/地点，如：深圳南山"
          @keyup.enter="saveWeatherAddress()"
          @focus="onWeatherInputFocus()"
          @blur="onWeatherInputBlur()"
          @keydown.esc="cancelWeatherAddressEdit()"
          @click.stop
        />
        <span
          v-else
          class="info-location-label"
          @click="editWeatherAddress()"
          title="点击切换地址"
        >
          📍 {{ weatherInfo?.location || weatherAddress }} <span class="info-location-edit">✏️</span>
        </span>
      </div>
    </div>

    <!-- 更新对话框：查到新版本时自动弹出（像正经软件那样） -->
    <div v-if="updateDialog" class="update-panel" :style="updateStyle">
      <div
        class="chat-header panel-drag-handle"
        title="按住这里可以拖动；双击恢复跟随桌宠"
        @pointerdown="onUpdateDragStart"
        @dblclick="resetUpdatePos"
      >
        <span>⬆️ 发现新版本</span>
        <button class="chat-close" :disabled="updateBusy" @click="closeUpdateDialog()">✕</button>
      </div>

      <div class="update-body">
        <div class="upd-vers">
          <span class="upd-old">{{ updateInfo.version }} · {{ formatBuildTime(updateInfo.build_epoch) }}</span>
          <span class="upd-arrow">→</span>
          <span class="upd-new">{{ updateFound.version }}</span>
        </div>
        <div class="upd-meta">
          安装包 {{ formatBytes(updateFound.size) }} ·
          {{ updateInfo.installed ? '装完自动重启' : '将打开安装向导' }}
        </div>

        <div v-if="updateFound.notes" class="update-notes">{{ updateFound.notes }}</div>

        <div v-if="updatePhase" class="upd-progress">
          <div class="upd-bar">
            <div class="upd-bar-fill" :style="{ width: (updatePhase === 'install' ? 100 : updatePercent) + '%' }"></div>
          </div>
          <div class="upd-progress-text">
            <template v-if="updatePhase === 'install'">正在安装……</template>
            <template v-else-if="updateTotal > 0">
              {{ updatePercent }}% · {{ formatBytes(updateGot) }} / {{ formatBytes(updateTotal) }}<span v-if="updateSpeed"> · {{ updateSpeed }}</span>
            </template>
            <template v-else>正在准备…</template>
          </div>
        </div>

        <div v-if="updateMsg" class="setting-test-result" :class="{ bad: updateBad }">{{ updateMsg }}</div>

        <div v-if="!updateBusy" class="upd-actions">
          <button class="setting-save" @click="installUpdate()">{{ updateBad ? '重试' : '立即更新' }}</button>
          <button class="upd-skip" @click="skipThisVersion()">跳过这个版本</button>
          <button class="upd-skip" @click="closeUpdateDialog()">稍后</button>
        </div>
      </div>
    </div>

    <!-- 上手说明面板（环形菜单第 8 格 📖 点开） -->
    <div v-if="showHelp" class="help-panel" :style="helpStyle">
      <div
        class="chat-header panel-drag-handle"
        title="按住这里可以把说明拖到任意位置；双击恢复跟随桌宠"
        @pointerdown="onHelpDragStart"
        @dblclick="resetHelpPos"
      >
        <span>📖 藿藿怎么用</span>
        <button class="chat-close" @click="toggleHelp()">✕</button>
      </div>

      <div class="help-body">
        <div class="help-sec">🖱 在她身上动手</div>
        <div class="help-row"><span class="help-key">单击</span><span class="help-desc">摸头；点别处随机换一个动作</span></div>
        <div class="help-row"><span class="help-key">双击</span><span class="help-desc">打开 / 收起聊天面板</span></div>
        <div class="help-row"><span class="help-key">右键</span><span class="help-desc">打开环形菜单（就是刚才那圈）</span></div>
        <div class="help-row"><span class="help-key">拖动</span><span class="help-desc">把她挪到屏幕任意位置</span></div>
        <div class="help-row"><span class="help-key">滚轮</span><span class="help-desc">放大 / 缩小</span></div>
        <div class="help-row"><span class="help-key">悬停 1 秒</span><span class="help-desc">冒出信息面：日期 / 节日 / 天气（含未来三天）</span></div>
        <div class="help-row help-danger">
          <span class="help-key">连点 4 下</span>
          <span class="help-desc">⚠️ <b>退出桌宠</b>。点两下是开聊天，别再顺着点下去 —— 她"站起来了又突然没了"多半就是这一下</span>
        </div>

        <div class="help-sec">💬 三种聊天模式<span class="help-hint">面板顶部按钮，或直接发口令</span></div>
        <div class="help-row"><span class="help-key">💬 聊天</span><span class="help-desc">闲聊、说心事；会记住关于你的事，有表情和语音</span></div>
        <div class="help-row"><span class="help-key">❓ 问答</span><span class="help-desc">直接给答案，可联网查资料，不扯闲篇</span></div>
        <div class="help-row"><span class="help-key">📁 文件</span><span class="help-desc">读写桌面 / 下载 / 文档里的文件；同一个项目会记住做到哪一步了</span></div>
        <div class="help-tip">
          切模式：<code>聊天模式</code> / <code>问答模式</code> / <code>文件模式</code><br />
          只让这一条走某个模式：<code>问答: 量子纠缠是什么</code>、<code>文件: 把桌面的笔记.txt 整理成要点</code><br />
          文件模式换了新活儿：发一句 <code>新项目</code> 清掉项目记忆
        </div>

        <div class="help-sec">📱 用手机遥控她</div>
        <div class="help-tip">
          1. 在设置里填 Telegram Bot Token（环形菜单中间的 ⚙️）<br />
          2. 手机给这个 bot 发一条消息，完成绑定<br />
          3. 以后在手机上发「启动」，哪怕桌宠关着也能把她叫起来<br />
          4. 手机说的话她会回；电脑收到新邮件也能同步到你手机
        </div>
        <div class="help-row">
          <span class="help-key">没反应？</span>
          <span class="help-desc">多半是梯子/代理断了（电脑上会弹一次提示）；标题栏 📱 悬停能看通道心跳</span>
        </div>

        <div class="help-sec">🌸 环形菜单其余几格</div>
        <div class="help-tip">
          <b>聊天</b> 开聊天框 · <b>动作</b> 随机来个动作 ·
          <b>QQ</b> / <b>酷狗</b> 一键打开对应程序（找不到或装在别处时，可以在设置里指定位置）·
          <b>花园</b> / <b>网址</b> / <b>BGM</b> 三格是<b>网址快捷键</b>：名字和网址都能在设置里改成你自己的
          （默认是动漫花园 / bgm.tv 的个人主页；没填网址的那格会画得暗一些，点它会提示去哪儿填）·
          中间 <b>⚙️</b> 是设置
        </div>

        <div class="help-sec">💡 还有这些</div>
        <div class="help-tip">
          · 平时点击是<b>穿透</b>到桌面的，鼠标移到她身上才接管，不用担心挡住别的东西<br />
          · 设置里可以给她设<b>全局快捷键</b>：随手一按就能开聊天框、让她做动作，或者把她藏起来/叫回来<br />
          · 五分钟没人理她，她会自己在屏幕上飘一会儿；你一动手就停<br />
          · 新邮件会弹在她旁边（设置里填邮箱与授权码）<br />
          · 填了火山引擎凭据，她就用自己的声音念出来，随时可以打断<br />
          · 设置里还能调大小、开机自启、开关看门狗（关掉桌宠也照样能用手机叫回来）
        </div>
      </div>
    </div>

    <!-- 设置面板（环形菜单中央 ⚙️ 点开） -->
    <div v-if="showSettings" class="settings-panel" :style="settingsStyle">
      <div
        class="chat-header panel-drag-handle"
        title="按住这里可以把设置面板拖到任意位置；双击恢复跟随桌宠"
        @pointerdown="onSettingsDragStart"
        @dblclick="resetSettingsPos"
      >
        <span>⚙️ 藿藿的设置<<span class="settings-ver">{{ settings.version ? 'v' + settings.version : '' }}</span></span>
        <button class="chat-close" @click="toggleSettings()">✕</button>
      </div>

      <div class="settings-body">
        <!-- 0) 版本更新：放最上面（面板已经很长，放底部等于藏起来）
             更新源是**内置**的，所以这里不再给输入框 —— 只报告"当前版本 + 从哪更新 + 检查"。 -->
        <div class="setting-row">
          <label class="setting-label">
            ⬆️ 版本更新
            <span class="setting-value">{{ updateInfo.version || '…' }} · {{ formatBuildTime(updateInfo.build_epoch) }}</span>
          </label>
          <div class="link-row">
            <span class="update-source" :title="updateInfo.update_url">
              <template v-if="updateInfo.update_url">🌐 更新源：{{ updateSourceHost }}</template>
              <template v-else>🌐 未配置更新源（改 settings.json 里的 update_url）</template>
            </span>
            <button class="link-open" :disabled="updateBusy" title="检查更新" @click.stop="checkUpdate()">⟳</button>
          </div>
          <div v-if="updateMsg" class="setting-test-result" :class="{ bad: updateBad }">{{ updateMsg }}</div>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: settings.update_auto_check }"
              :disabled="settingsSaving"
              @click.stop="settings.update_auto_check = !settings.update_auto_check"
            >
              {{ settings.update_auto_check ? '已开启' : '已关闭' }}
            </button>
            <span class="setting-switch-label">启动时自动检查更新（查到才弹窗）</span>
          </div>
          <div class="setting-hint">
            当前是{{ updateInfo.installed ? '安装版（可静默升级，装完自动重启）' : '绿色版（从文件夹直接运行，更新会打开安装向导）' }}。
            点 ⟳ 查到新版本会<b>自动弹窗</b>；更新包先验 Ed25519 签名、再核对 sha256，验不过一律拒绝安装。
            更新地址已随程序内置，不需要填；想在别处测试新版本时，
            手工改 <code>settings.json</code> 里的 <code>update_url</code> 即可（面板不再提供输入框）。
          </div>
        </div>

        <!-- 1) 🤖 模型 API：聊天 / 问答 / 文件三个模式共用这一份配置 -->
        <div class="setting-row">
          <label class="setting-label">
            🤖 模型 API
            <span class="setting-value">{{ settings.llm_model_effective }}</span>
          </label>
          <div class="setting-inline-row">
            <span class="setting-inline-label">服务商</span>
            <select
              v-model="settings.llm_provider"
              class="setting-select llm-provider"
              @click.stop
              @change="onLlmProviderChange()"
            >
              <option v-for="p in (settings.llm_providers || [])" :key="p.id" :value="p.id">
                {{ p.label }}
              </option>
            </select>
          </div>
          <div class="setting-key-row">
            <input
              v-model="settings.llm_api_key"
              :type="showApiKey ? 'text' : 'password'"
              class="setting-input llm-key"
              placeholder="sk-...（填你自己的密钥）"
              @click.stop
            />
            <button class="setting-eye" @click.stop="showApiKey = !showApiKey" :title="showApiKey ? '隐藏' : '显示'">
              {{ showApiKey ? '🙈' : '👁' }}
            </button>
          </div>
          <!-- 接口地址：预设都自带默认地址，所以平时收起来只显示"正在用哪条"；
               选「自定义」、或者已经存了一条自定义地址时，输入框自动露出来。
               已经存了地址的情况**必须**露出来 —— 藏起来的输入框等于一个看不见的配置。 -->
          <div v-if="showBaseUrlInput" class="setting-inline-row">
            <span class="setting-inline-label">接口地址</span>
            <input
              v-model="settings.llm_base_url"
              class="setting-input llm-base-url"
              :placeholder="llmProvider?.base_url || 'https://api.xxx.com/v1（自己填）'"
              @click.stop
            />
          </div>
          <div v-else class="setting-hint llm-endpoint-row">
            接口地址：<code class="llm-endpoint">{{ settings.llm_base_url_effective }}</code>
            （用 {{ settings.llm_label }} 的默认地址）
            <button class="llm-link-btn" @click.stop="baseUrlOverride = true">用别的地址</button>
          </div>
          <div class="setting-inline-row">
            <span class="setting-inline-label">模型</span>
            <input
              v-model="settings.llm_model"
              class="setting-input llm-model"
              list="llm-model-choices"
              :placeholder="llmProvider?.model || '模型名（自己填）'"
              @click.stop
            />
            <datalist id="llm-model-choices">
              <option v-for="m in (llmProvider?.models || [])" :key="m" :value="m"></option>
            </datalist>
          </div>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: settings.llm_use_proxy }"
              @click.stop="settings.llm_use_proxy = !settings.llm_use_proxy"
            >
              走系统代理：{{ settings.llm_use_proxy ? '开' : '关' }}
            </button>
            <span class="setting-switch-label">
              OpenAI / OpenRouter / Groq 在国内要开；DeepSeek、通义这类直连更快更稳
            </span>
          </div>
          <div class="setting-hint">
            <b>聊天 / 问答 / 文件三个模式共用这一个模型</b> —— 换服务商只改这里。
            地址留空 = 用所选服务商的默认地址，模型留空 = 用它的默认模型。
            程序里**不内置**任何密钥，都只存在本机 <code>settings.json</code>。
          </div>
          <div class="setting-hint">
            当前生效：<code>{{ settings.llm_summary }}</code>
          </div>
          <div class="setting-switch-row" style="margin-top: 6px;">
            <button
              class="setting-switch"
              :disabled="llmTesting || !settings.llm_api_key.trim()"
              @click.stop="testLlm()"
            >
              {{ llmTesting ? '测试中…' : '🧪 测试连接' }}
            </button>
            <span class="setting-switch-label">会先保存再发一个最小请求，当场告诉你卡在密钥、地址还是模型名</span>
          </div>
          <div v-if="llmTestMsg" class="setting-test-result" :class="{ bad: !llmTestOk }">{{ llmTestMsg }}</div>
          <div v-if="!settings.has_api_key" class="setting-warn">
            ⚠️ 还没填密钥 —— 聊天 / 问答 / 文件模式现在都用不了
          </div>
        </div>

        <!-- 性格：自定义人设。追加在提示词末尾并声明优先，硬规则不受影响 -->
        <div class="setting-row">
          <label class="setting-label">
            🎭 性格
            <span class="setting-value">{{ (settings.persona || '').length }}/{{ settings.persona_max }}</span>
          </label>
          <textarea
            v-model="settings.persona"
            class="setting-input persona-input"
            rows="4"
            :maxlength="settings.persona_max"
            placeholder="留空 = 用内置的藿藿人设。想换个性子就写在这儿，例如：你是一只高冷的猫娘，说话极简、只用短句，被夸的时候会扭过头去。"
            @click.stop
          ></textarea>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: !!settings.persona.trim() }"
              :disabled="!settings.persona.trim() || settingsSaving"
              @click.stop="settings.persona = ''"
            >
              {{ settings.persona.trim() ? '点此清空' : '内置人设' }}
            </button>
            <span class="setting-switch-label">{{ settings.persona.trim() ? '当前用你写的性格' : '当前用内置的藿藿人设' }}</span>
          </div>
          <div class="setting-hint">
            这段话会<b>追加在系统提示词的末尾</b>并声明优先，所以能盖过内置的藿藿语气；
            但工具用法、文件白名单、<code>[emotion:xxx]</code> 情绪标签、回复长度这些<b>硬性规则不受影响</b>
            —— 换成别的性格也不会让她乱调工具、或者丢了表情。
            三种聊天模式和手机 Telegram 上都生效；保存后下一条消息就变。
          </div>
        </div>

        <!-- 2) 桌宠大小 -->
        <div class="setting-row">
          <label class="setting-label">
            📏 桌宠大小
            <span class="setting-value">{{ Math.round(settings.pet_scale / settings.pet_scale_default * 100) }}%</span>
          </label>
          <input
            type="range"
            class="setting-range"
            :min="settings.pet_scale_min"
            :max="settings.pet_scale_max"
            step="0.001"
            :value="settings.pet_scale"
            @input="applyPetScale(Number(($event.target as HTMLInputElement).value))"
            @click.stop
          />
          <div class="setting-hint">拖动即时预览，保存后生效。也可以直接在宠物身上滚滚轮</div>
        </div>

        <!-- 3) 天气地点 -->
        <div class="setting-row">
          <label class="setting-label">📍 天气地点</label>
          <input
            v-model="settings.weather_location"
            class="setting-input"
            placeholder="如：广州 / 北京朝阳区"
            @click.stop
          />
          <div class="setting-hint">填了之后悬浮弹窗和她回答天气都以这里为准，不用带地名</div>
        </div>

        <!-- 4) 豆包语音 -->
        <div class="setting-row">
          <label class="setting-label">🔊 豆包语音（火山引擎）</label>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: settings.volc_enabled }"
              @click.stop="setVolcEnabled(!settings.volc_enabled)"
            >
              {{ settings.volc_enabled ? '已开启' : '已关闭' }}
            </button>
            <span class="setting-switch-label">开启后她说话用豆包的声音</span>
          </div>
          <input
            v-model="settings.volc_app_id"
            class="setting-input"
            placeholder="App ID（火山引擎语音控制台的数字 ID）"
            @click.stop
          />
          <div class="setting-key-row" style="margin-top: 6px;">
            <input
              v-model="settings.volc_access_token"
              :type="showVolcToken ? 'text' : 'password'"
              class="setting-input"
              placeholder="Access Token"
              @click.stop
            />
            <button class="setting-eye" @click.stop="showVolcToken = !showVolcToken" :title="showVolcToken ? '隐藏' : '显示'">
              {{ showVolcToken ? '🙈' : '👁' }}
            </button>
          </div>
          <div class="setting-hint">
            在火山引擎「语音技术」控制台创建应用后拿到。<b>两个都要填</b>才算配好。
            不填也能用，只是会退回系统语音 
          </div>
          <div v-if="settings.volc_enabled && !settings.has_volc_credentials" class="setting-warn">
            ⚠️ 开着语音但凭据不全 —— 现在会退回系统语音。
          </div>
        </div>

        <!-- 5) Telegram 机器人（换 bot 只需要改这里） -->
        <div class="setting-row">
          <label class="setting-label">🤖 Telegram Bot Token</label>
          <div class="setting-key-row">
            <input
              v-model="settings.telegram_token"
              :type="showTgToken ? 'text' : 'password'"
              class="setting-input"
              placeholder="123456789:AAH..."
              @click.stop
            />
            <button class="setting-eye" @click.stop="showTgToken = !showTgToken" :title="showTgToken ? '隐藏' : '显示'">
              {{ showTgToken ? '🙈' : '👁' }}
            </button>
          </div>
          <div class="setting-hint">
            换机器人：把新 bot 的 token 贴进来 → 保存。**无需重启**，约 25 秒内自动切换
            （轮询是长连接，等这一轮返回就换）。 
            绑定关系会保留（chat_id 是你自己的，与 bot 无关），update 进度会自动归零。
          </div>
          <div v-if="!settings.has_telegram_token" class="setting-warn">
            ⚠️ 还没填 Token —— 手机 Telegram 通道不会启动
          </div>
        </div>

        <!-- 6) 收件邮箱（原来地址和授权码是写死在源码里的） -->
        <div class="setting-row">
          <label class="setting-label">
            📧 收件邮箱
            <span class="setting-value">{{ settings.has_email_config ? '已启用' : '未配置' }}</span>
          </label>
          <input
            v-model="settings.email_addr"
            class="setting-input"
            placeholder="你的邮箱，如 xxxx@qq.com"
            @click.stop
          />
          <div class="setting-key-row" style="margin-top: 6px;">
            <input
              v-model="settings.email_auth"
              :type="showEmailAuth ? 'text' : 'password'"
              class="setting-input"
              placeholder="IMAP 授权码（不是登录密码）"
              @click.stop
            />
            <button class="setting-eye" @click.stop="showEmailAuth = !showEmailAuth" :title="showEmailAuth ? '隐藏' : '显示'">
              {{ showEmailAuth ? '🙈' : '👁' }}
            </button>
          </div>
          <div class="setting-inline-row" style="margin-top: 6px;">
            <span class="setting-inline-label">服务器</span>
            <select v-model="imapChoice" class="setting-select" @click.stop @change="onImapChoiceChange">
              <option value="auto">
                自动识别{{ settings.imap_known ? '（' + settings.imap_known + '）' : '（按邮箱域名）' }}
              </option>
              <option v-for="c in (settings.imap_choices || [])" :key="c.server" :value="c.server">
                {{ c.label }}
              </option>
              <option value="custom">其他（手工填写）…</option>
            </select>
          </div>
          <input
            v-if="imapChoice === 'custom'"
            v-model="settings.imap_server"
            class="setting-input"
            style="margin-top: 6px;"
            placeholder="如 imap.your-corp.com（端口固定 993 + SSL）"
            @click.stop
          />
          <div class="setting-hint">
            授权码在「邮箱网页版 → 设置 → 账户」里开启 IMAP 后生成。
            （QQ 邮箱叫「授权码」，163 叫「客户端授权密码」），<b>不是登录密码</b>。<br />
            服务器一般选「自动识别」就行；企业邮箱或自建邮箱选「其他」手工填
            <template v-if="settings.imap_server_effective">
              。当前使用 <code>{{ settings.imap_server_effective }}</code>
            </template>
          </div>
          <div class="setting-switch-row" style="margin-top: 6px;">
            <button
              class="setting-switch"
              :disabled="emailTesting || !settings.email_addr.trim() || !settings.email_auth.trim()"
              @click.stop="testEmail()"
            >
              {{ emailTesting ? '测试中…' : '测试连接' }}
            </button>
            <span class="setting-switch-label">会先保存再连一次，当场告诉你能不能用</span>
          </div>
          <div v-if="emailTestMsg" class="setting-test-result" :class="{ bad: emailTestBad }">{{ emailTestMsg }}</div>
          <div v-if="!settings.email_addr.trim() || !settings.email_auth.trim()" class="setting-warn">
            ⚠️ 邮箱地址和授权码<b>两个都要填</b>才会启用 —— 新邮件会弹在桌宠旁边，并同步到手机。
          </div>
          <div
            v-else-if="!settings.imap_server.trim() && !settings.imap_known"
            class="setting-warn"
          >
            ⚠️ 认不出这个邮箱的 IMAP 服务器，请在上面手工填写，否则连不上
          </div>
        </div>

        <!-- 7) 文件模式的项目记忆 -->
        <div class="setting-row">
          <label class="setting-label">
            📁 文件模式的项目记忆
            <span class="setting-value">{{ projectMemoryCount }} 条</span>
          </label>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: projectMemoryCount > 0 }"
              :disabled="projectMemoryCount === 0"
              @click.stop="clearProjectMemory()"
            >
              {{ projectMemoryCount > 0 ? '清空重开' : '已是空的' }}
            </button>
            <span class="setting-switch-label">她记得这个项目做到哪、文件在哪</span>
          </div>
          <div class="setting-hint">
            说「新项目」「重开」「忘记这件事」也能清空，手机 Telegram 上同样有效。 
            清空后她不会再带着上一个项目的内容回答
          </div>
        </div>

        <!-- 8) 文件模式的处理能力（模型 / 步数上限） -->
        <div class="setting-row">
          <label class="setting-label">🧠 文件模式的处理能力</label>
          <div class="setting-hint">
            模型在「🤖 模型 API」里统一设置（三个模式共用），这里只调它一次能干多少步
          </div>
          <div class="setting-inline-row">
            <span class="setting-inline-label">最多步数</span>
            <input
              type="range"
              class="setting-range"
              :min="settings.max_rounds_min"
              :max="settings.max_rounds_max"
              step="1"
              :value="settings.max_rounds"
              @input="settings.max_rounds = Number(($event.target as HTMLInputElement).value)"
              @click.stop
            />
            <span class="setting-value">{{ settings.max_rounds }} 步</span>
          </div>
          <div class="setting-hint">
            步数是「读文件 / 写文件 / 执行命令」这类动作的次数上限，用完她会停下来汇报做到哪一步。 
            复杂的活儿（多份文件、要来回改）可以调到 20 步以上。 
            模型越强，多步操作里越不容易跑偏 —— 想更快更省就换模型（在 🤖 模型 API 里） 
          </div>
        </div>

        <!-- 9) 开机自启 -->
        <div class="setting-row">
          <label class="setting-label">🚀 开机自启动</label>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: settings.autostart_pet }"
              :disabled="settingsSaving"
              @click.stop="toggleAutostart('pet')"
            >
              {{ settings.autostart_pet ? '已开启' : '已关闭' }}
            </button>
            <span class="setting-switch-label">桌宠（登录即出现）</span>
          </div>
          <div class="setting-switch-row">
            <button
              class="setting-switch"
              :class="{ on: settings.autostart_watchdog }"
              :disabled="settingsSaving"
              @click.stop="toggleAutostart('watchdog')"
            >
              {{ settings.autostart_watchdog ? '已开启' : '已关闭' }}
            </button>
            <span class="setting-switch-label">看门狗（关掉桌宠后也能用手机叫回来）</span>
          </div>
          <div class="setting-hint">状态直接读启动文件夹里的快捷方式，你手工删掉也会如实显示</div>
        </div>

        <!-- 环形菜单的三格网址快捷键 -->
        <div class="setting-row">
          <label class="setting-label">🔗 网址快捷键<span class="setting-value">环形菜单第 4/5/6 格</span></label>
          <div v-for="(l, i) in settings.links" :key="'link' + i" class="link-row">
            <input
              v-model="l.name"
              class="setting-input link-name"
              :maxlength="settings.link_name_max"
              placeholder="名称"
              @click.stop
            />
            <input
              v-model="l.url"
              class="setting-input"
              placeholder="https://…（留空 = 这格空着）"
              @click.stop
            />
            <button
              class="link-open"
              :disabled="!l.url.trim()"
              title="先试着打开这个网址（不用保存）"
              @click.stop="testLink(l.url)"
            >↗</button>
          </div>
          <div class="setting-hint">
            三格都是网址捷径，点第一格的图标就是打开它。默认填的是动漫花园 / bgm.tv 的<b>个人主页</b>地址，
            换成你自己的即可；名称最多 {{ settings.link_name_max }} 个字（饼图一格就这么点地方）。
            中间那格原来是「漂浮」开关，现在也是网址位 —— 留空时饼图上那格会变暗，点它会提示来这儿填。
          </div>
        </div>

        <!-- 本地程序位置：QQ / 酷狗（支持把桌面快捷方式直接拖进来） -->
        <div class="setting-row">
          <label class="setting-label">🖥 程序位置<span class="setting-value">留空 = 自动查找</span></label>
          <div class="link-row">
            <span class="path-name">QQ</span>
            <input
              v-model="settings.qq_path"
              class="setting-input"
              data-drop-target="qq_path"
              placeholder="把桌面快捷方式拖进来，或手填 …\QQ.exe"
              @click.stop
            />
            <button
              class="link-open"
              :disabled="!settings.qq_path.trim() || settingsSaving"
              title="按这个位置启动一次 QQ"
              @click.stop="testProgram('qq_path')"
            >↗</button>
          </div>
          <div class="link-row">
            <span class="path-name">酷狗</span>
            <input
              v-model="settings.kugou_path"
              class="setting-input"
              data-drop-target="kugou_path"
              placeholder="把桌面快捷方式拖进来，或手填 …\KuGou.exe"
              @click.stop
            />
            <button
              class="link-open"
              :disabled="!settings.kugou_path.trim() || settingsSaving"
              title="按这个位置启动一次酷狗（会开始播放）"
              @click.stop="testProgram('kugou_path')"
            >↗</button>
          </div>
          <div class="setting-hint">
            留空就按老办法自动找：常见路径 → 注册表 → 开始菜单 → 全盘搜。
            <b>直接把桌面上的 QQ / 酷狗快捷方式拖进框里</b>最省事，会自动解析成它指向的 .exe；
            填了但文件已经不在（重装到别处）会自动退回自动查找，不会变成"点了没反应"。
            保存时会检查这个位置真有 .exe，填错当场告诉你。
          </div>
        </div>

        <!-- 全局快捷键：点一下框再按组合键就录进去 -->
        <div class="setting-row">
          <label class="setting-label">⌨️ 全局快捷键<span class="setting-value">留空 = 不启用</span></label>
          <div
            v-for="hk in [
              { field: 'hotkey_chat', label: '聊天面板开关' },
              { field: 'hotkey_toggle', label: '隐藏 / 显示桌宠' },
              { field: 'hotkey_action', label: '让她做个动作' },
            ]"
            :key="hk.field"
            class="hotkey-row"
          >
            <span class="hotkey-label">{{ hk.label }}</span>
            <input
              class="setting-input hotkey-input"
              :class="{ recording: hotkeyRecording === hk.field }"
              :value="(settings as any)[hk.field]"
              :placeholder="hotkeyRecording === hk.field ? '请按组合键…' : '点这里再按组合键'"
              readonly
              @click.stop
              @focus="hotkeyRecording = hk.field"
              @blur="hotkeyRecording = ''"
              @keydown="captureHotkey($event, hk.field)"
            />
            <button
              class="link-open"
              :disabled="!(settings as any)[hk.field]"
              title="清空这一项"
              @click.stop="clearHotkey(hk.field)"
            >✕</button>
          </div>
          <div v-if="hotkeyStatus.length" class="hotkey-status">
            <div
              v-for="s in hotkeyStatus"
              :key="s.id"
              :class="['hotkey-item', s.ok ? 'ok' : (s.error ? 'bad' : '')]"
            >
              <!-- 先判 error：注册失败时后端不会把它算进"已生效"，accel 是空的。
                   原来先判 accel，失败那条就会掉进"未设置"分支 —— 面板等于在撒谎。 -->
              <template v-if="s.error">⚠️ {{ s.label }}：{{ s.error }}</template>
              <template v-else-if="s.accel">✅ {{ s.label }}：{{ s.accel }}</template>
              <template v-else>· {{ s.label }}：未设置</template>
            </div>
          </div>
          <div v-if="hotkeyMsg" class="setting-test-result bad">{{ hotkeyMsg }}</div>
          <div class="setting-hint">
            这几个是<b>全局</b>快捷键：不管当前在哪个软件里，按下都作用到桌宠。
            点输入框再按组合键即可；<b>最好带 Ctrl / Alt</b> —— 只按单个字母会把正常打字抢走，
            所以这种写法会被拒绝。「隐藏 / 显示桌宠」藏起来后用同一个键叫回来。
            保存后立即生效；组合键被别的软件占用的话，上面会直接写出来。
          </div>
        </div>
        <!-- 和风天气凭据：程序里不再内置，谁的额度谁自己填 -->
        <div class="setting-row">
          <label class="setting-label">
            🌦 天气接口
            <span class="setting-value">{{ settings.has_qweather ? '已配置' : '未配置（悬浮弹窗不显示天气）' }}</span>
          </label>
          <div class="link-row">
            <span class="path-name">Host</span>
            <input v-model="settings.qweather_host" class="setting-input" placeholder="xxx.qweatherapi.com" @click.stop />
          </div>
          <div class="link-row">
            <span class="path-name">项目</span>
            <input v-model="settings.qweather_project_id" class="setting-input" placeholder="项目ID" @click.stop />
          </div>
          <div class="link-row">
            <span class="path-name">凭据</span>
            <input v-model="settings.qweather_credential_id" class="setting-input" placeholder="凭据ID" @click.stop />
          </div>
          <div class="link-row">
            <span class="path-name">私钥</span>
            <input
              v-model="settings.qweather_key_seed"
              class="setting-input"
              type="password"
              :placeholder="settings.has_qweather_seed ? '要更换就重新填一次，留空=不改' : '64 位十六进制私钥种子'"
              @click.stop
            />
          </div>
          <!-- 私钥是**只写**的（后端不回传原文），所以必须显式告诉用户"它还在" ——
               否则每次打开面板那个框都是空的，看着就像"填了又丢了"（真实反馈）。 -->
          <div v-if="settings.has_qweather_seed && !settings.qweather_key_seed" class="setting-hint seed-saved">
            ✅ 私钥已保存：<code>{{ settings.qweather_seed_hint }}</code>
            —— 出于安全不回显原文（要更换就重新填一次，留空表示不改）
          </div>
          <div v-else-if="!settings.has_qweather_seed && !settings.qweather_key_seed" class="setting-hint">
            ⚠️ 还没填私钥种子 —— 四项齐了天气才可用
          </div>
          <div class="setting-hint">
            在 <b>dev.qweather.com</b> 免费申请：设置页复制专属 API Host；项目管理复制项目ID；
            「添加凭据」选 <b>JSON Web Token</b>、粘贴公钥、复制凭据ID；私钥种子填在这里。
            <b>这些原来写死在程序里</b>（包括私钥），等于拿到安装包的人都能用你的额度 ——
            现在只存在你自己的设置文件里，安装包不再带任何凭据。
          </div>
        </div>
      </div>

      <div class="settings-foot">
        <span v-if="settingsMsg" class="settings-msg">{{ settingsMsg }}</span>
        <span v-else-if="settingsLoaded" class="settings-src" :title="settingsFile">
          ✅ 已从 settings.json 读取
        </span>
        <button class="setting-save" :disabled="settingsSaving || !settingsLoaded" @click.stop="saveSettings()">
          {{ !settingsLoaded ? '加载中…' : (settingsSaving ? '处理中…' : '保存') }}
        </button>
      </div>
    </div>

    <!-- 右键饼状菜单 -->
    <div v-if="pieMenuVisible" class="pie-overlay" @click="hidePieMenu" @contextmenu.prevent="hidePieMenu">
      <div class="pie-container" :style="{ left: pieMenuPos.x + 'px', top: pieMenuPos.y + 'px' }" @click.stop>
        <svg width="250" height="250" viewBox="-125 -125 250 250">
          <g v-for="(item, i) in pieMenuItems" :key="i">
            <path
              :d="sectorPaths[i]"
              :fill="pieMenuHovered === i ? 'rgba(255,255,255,0.38)' : (item.off ? 'rgba(20,20,36,0.45)' : 'rgba(30,30,55,0.72)')"
              stroke="rgba(255,255,255,0.28)"
              stroke-width="1.5"
              style="cursor: pointer; transition: fill 0.1s;"
              @mouseenter="pieMenuHovered = i"
              @mouseleave="pieMenuHovered = -1"
              @click.stop="onPieItemClick(i)"
            />
            <!-- 图标：有自定义图片用 <image>，否则用 emoji -->
            <image v-if="item.image"
              :href="item.image"
              :x="labelPositions[i].x - 14" :y="labelPositions[i].y - 18"
              width="28" height="28"
              style="pointer-events: none;"
            />
            <text v-else
              :x="labelPositions[i].x" :y="labelPositions[i].y"
              text-anchor="middle" dominant-baseline="central"
              fill="white" style="pointer-events: none;"
            >
              <tspan :x="labelPositions[i].x" dy="-7" font-size="22">{{ item.icon }}</tspan>
            </text>
            <!-- 标签文字 -->
            <text
              :x="labelPositions[i].x" :y="labelPositions[i].y"
              text-anchor="middle" dominant-baseline="central"
              fill="white" style="pointer-events: none;"
            >
              <tspan :x="labelPositions[i].x" :dy="item.image ? 18 : 21" font-size="11" :fill="item.off ? 'rgba(255,255,255,0.45)' : 'rgba(255,255,255,0.85)'" font-weight="600">{{ item.label }}</tspan>
            </text>
          </g>
          <!-- 七条分割线 -->
          <g v-for="(_, i) in pieMenuItems" :key="'line'+i" style="pointer-events: none;">
            <line
              :x1="0" :y1="0"
              :x2="OUTER_R * Math.cos(-Math.PI/2 + i * ANGLE_PER_SECTION)"
              :y2="OUTER_R * Math.sin(-Math.PI/2 + i * ANGLE_PER_SECTION)"
              stroke="rgba(255,255,255,0.25)" stroke-width="1"
            />
          </g>
          <!-- 中心：设置按钮（原来是个纯装饰的 🦊） -->
          <g
            style="cursor: pointer;"
            @mouseenter="pieCenterHovered = true"
            @mouseleave="pieCenterHovered = false"
            @click.stop="openSettings()"
          >
            <circle
              cx="0" cy="0" :r="INNER_R"
              :fill="pieCenterHovered ? 'rgba(255,255,255,0.30)' : 'rgba(15,15,30,0.72)'"
              stroke="rgba(255,255,255,0.30)" stroke-width="2"
              style="transition: fill 0.1s;"
            />
            <text x="0" y="0" text-anchor="middle" dominant-baseline="central" font-size="22" style="pointer-events: none;">⚙️</text>
            <text x="0" y="0" text-anchor="middle" dominant-baseline="central" style="pointer-events: none;">
              <tspan x="0" dy="17" font-size="9" fill="rgba(255,255,255,0.85)" font-weight="600">设置</tspan>
            </text>
          </g>
        </svg>
      </div>
    </div>
  </div>
</template>

<style scoped>
.app { width: 100%; height: 100%; background: transparent; position: relative; pointer-events: auto; }
canvas { display: block; width: 100%; height: 100%; pointer-events: auto; }

.chat-panel {
  position: absolute; width: 280px; background: rgba(20,20,40,0.97); border: 2px solid rgba(255,100,100,0.8);
  border-radius: 14px; padding: 14px; z-index: 99999; pointer-events: auto; color: #fff;
}
.chat-header { display: flex; justify-content: space-between; align-items: center; color: #ccc; font-size: 13px; margin-bottom: 8px; }
.chat-header-actions { display: flex; align-items: center; gap: 4px; }
.chat-toggle-btn {
  border: 1px solid rgba(255,255,255,0.25); border-radius: 6px;
  padding: 2px 8px; font-size: 12px; cursor: pointer; line-height: 1.4;
  white-space: nowrap;
}
.tg-indicator { background: rgba(80,180,255,0.2); color: #a8d8ff; }
.tg-indicator:hover { background: rgba(80,180,255,0.35); }
.tg-indicator.off { opacity: 0.45; }
.chat-header { padding-right: 4px; }
.chat-close {
  background: rgba(255,255,255,0.08); border: none; color: #999; cursor: pointer;
  font-size: 13px; width: 22px; height: 22px; border-radius: 50%;
  display: flex; align-items: center; justify-content: center; line-height: 1;
  margin-left: 6px; flex-shrink: 0;
}
.chat-close:hover { background: rgba(255,80,80,0.3); color: #ff4444; }
.chat-reply { color: #e0e0ff; font-size: 13px; line-height: 1.6; margin-bottom: 10px; padding: 8px; background: rgba(255,255,255,0.06); border-radius: 8px; min-height: 24px; word-break: break-word; }
/* 文件模式进度条：一闪一闪的小圆点 + 当前动作，让长活儿看起来"在动" */
.chat-progress {
  display: flex; align-items: center; gap: 6px;
  font-size: 11px; color: #9fd0ff; margin: -4px 0 8px; padding: 5px 8px;
  background: rgba(90,170,255,0.10); border: 1px solid rgba(120,190,255,0.28);
  border-radius: 6px; word-break: break-word;
}
.chat-progress-dot {
  flex-shrink: 0; width: 6px; height: 6px; border-radius: 50%;
  background: #7cc4ff; animation: chat-progress-pulse 1s ease-in-out infinite;
}
@keyframes chat-progress-pulse {
  0%, 100% { opacity: 0.25; transform: scale(0.8); }
  50% { opacity: 1; transform: scale(1.15); }
}
/* 三模式切换条 */
.mode-bar { display: flex; gap: 6px; margin-bottom: 6px; }
.mode-btn {
  flex: 1; padding: 4px 0; border-radius: 8px; border: 1px solid rgba(255,255,255,0.2);
  background: rgba(255,255,255,0.06); color: #bbb; font-size: 12px; cursor: pointer;
  white-space: nowrap; transition: all 0.15s;
}
.mode-btn:hover { background: rgba(255,255,255,0.12); }
.mode-btn.active { background: rgba(74,108,247,0.35); border-color: #4a6cf7; color: #fff; }
.mode-hint { color: #888; font-size: 11px; margin-bottom: 8px; }
.chat-input-row { display: flex; gap: 6px; align-items: flex-end; }
.chat-input {
  flex: 1; padding: 6px 10px; border-radius: 8px; border: 1px solid rgba(255,255,255,0.2);
  background: rgba(255,255,255,0.08); color: #fff; font-size: 13px; outline: none;
  font-family: inherit; line-height: 1.5; display: block;
  resize: none;                /* 不要拖拽手柄，高度完全由内容驱动 */
  /* 单行高度 = 文字 19.5 + 内边距 12 + 边框 2 = 34px。
     小于这个值会让首帧就多出一条滚动条（border-box 下 30px 装不下一行） */
  min-height: 34px;
  overflow-x: hidden;           /* 长单词/长链接不产生横向滚动条 */
  overflow-y: auto;             /* 仅当高度触到窗口底部时才会出现纵向滚动条 */
  white-space: pre-wrap;        /* 按字符换行，长串也不会溢出 */
  overflow-wrap: break-word;
  word-break: break-word;
}
.chat-input::placeholder { color: #666; }
.chat-input:disabled { opacity: 0.6; }
.chat-input-row button { padding: 6px 14px; border-radius: 8px; border: none; background: #4a6cf7; color: #fff; font-size: 13px; cursor: pointer; white-space: nowrap; }
.chat-input-row button:hover { background: #5b7df8; }
.chat-input-row button:disabled { opacity: 0.5; cursor: default; }

.idle-bubble {
  position: absolute; max-width: 240px; padding: 12px 16px;
  background: #e8f4fd; border: 2px solid #b8d8f0; border-radius: 20px;
  color: #4a8c5c; font-size: 14px; font-weight: 600; line-height: 1.7;
  text-align: center; z-index: 100000; pointer-events: none;
  white-space: pre-wrap; word-break: break-word; box-shadow: 0 2px 8px rgba(0,0,0,0.08);
}
.idle-bubble::after {
  content: ''; position: absolute; bottom: -10px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0; border-left: 10px solid transparent; border-right: 10px solid transparent; border-top: 10px solid #e8f4fd;
}
.email-bubble {
  position: absolute; max-width: 300px; padding: 14px 18px;
  background: #fff8e1; border: 2px solid #f9a825; border-radius: 20px;
  color: #5d4037; font-size: 13px; font-weight: 500; line-height: 1.7;
  text-align: left; z-index: 100001; pointer-events: auto;
  white-space: pre-wrap; word-break: break-word; box-shadow: 0 2px 12px rgba(0,0,0,0.12);
}
.email-copy-btn {
  position: absolute; top: 6px; right: 8px;
  background: rgba(0,0,0,0.06); border: none; border-radius: 6px;
  font-size: 14px; cursor: pointer; padding: 2px 6px; line-height: 1;
}
.email-copy-btn:hover { background: rgba(0,0,0,0.15); }
.email-bubble::after {
  content: ''; position: absolute; bottom: -10px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0;
  border-left: 10px solid transparent; border-right: 10px solid transparent; border-top: 10px solid #fff8e1;
}
.email-bubble::before {
  content: ''; position: absolute; bottom: -13px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0;
  border-left: 11px solid transparent; border-right: 11px solid transparent; border-top: 11px solid #f9a825;
}

.idle-bubble::before {
  content: ''; position: absolute; bottom: -13px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0; border-left: 11px solid transparent; border-right: 11px solid transparent; border-top: 11px solid #b8d8f0;
}

/* 设置面板 */
.settings-panel {
  position: absolute; width: 340px;
  background: rgba(10,10,25,0.95); border: 1px solid rgba(120,200,255,0.4);
  border-radius: 10px; padding: 0; z-index: 100002; pointer-events: auto; color: #fff;
  display: flex; flex-direction: column; overflow: hidden;
  backdrop-filter: blur(12px); -webkit-backdrop-filter: blur(12px);
  box-shadow: 0 6px 24px rgba(0,0,0,0.45);
}
.settings-panel .chat-header { padding: 10px 14px 6px; margin-bottom: 0; }
/* 拖动把手：光标变移动、禁掉文本选中（不然拖着拖着会选中标题文字）。 
   关掉触摸手势（否则 pointermove 会被浏览器当成滚动/缩放吞掉。 */
.panel-drag-handle {
  cursor: move;
  user-select: none;
  -webkit-user-select: none;
  touch-action: none;
}
.panel-drag-handle:active { background: rgba(255,255,255,0.06); }
/* 关闭按钮不该继承"可拖动"的光标 */
.panel-drag-handle .chat-close { cursor: pointer; }
.settings-body {
  flex: 1; overflow-y: auto; padding: 4px 14px 10px;
  scrollbar-width: thin; scrollbar-color: rgba(255,255,255,0.2) transparent;
}
.settings-body::-webkit-scrollbar { width: 4px; }
.settings-body::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.2); border-radius: 2px; }
.setting-row { margin-bottom: 14px; }
.setting-label {
  display: flex; justify-content: space-between; align-items: center;
  font-size: 12px; color: #b9d9ff; margin-bottom: 5px; font-weight: 600;
}
.setting-value { color: #ffd54f; font-weight: 700; }
.setting-input {
  width: 100%; box-sizing: border-box;
  background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.18);
  border-radius: 6px; color: #fff; padding: 6px 8px; font-size: 12px;
}
.setting-input:focus { outline: none; border-color: rgba(120,200,255,0.6); }
.setting-key-row { display: flex; gap: 6px; align-items: center; }
.setting-eye {
  flex-shrink: 0; background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.18);
  border-radius: 6px; color: #fff; cursor: pointer; padding: 5px 8px; font-size: 12px; line-height: 1;
}
.setting-range { width: 100%; accent-color: #7cc4ff; }
.setting-inline-row { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
.setting-inline-label { flex-shrink: 0; font-size: 11px; color: #9aa8bd; width: 52px; }
.setting-select {
  flex: 1; box-sizing: border-box; font-size: 12px; padding: 5px 6px;
  background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.18);
  border-radius: 6px; color: #fff; cursor: pointer;
}
.setting-select option { background: #263043; color: #fff; }
.setting-hint { font-size: 10px; color: #8a9ab0; margin-top: 4px; line-height: 1.5; }
.setting-warn {
  font-size: 11px; color: #ffb4b4; background: rgba(255,90,90,0.14);
  border: 1px solid rgba(255,120,120,0.35); border-radius: 6px;
  padding: 5px 8px; margin-top: 6px; line-height: 1.5;
}
.setting-hint code { color: #cfe4ff; background: rgba(255,255,255,0.08); padding: 0 3px; border-radius: 3px; }
/* 连接测试结果：成功绿、失败红，带换行（后端给的是多行人话） */
.setting-test-result {
  font-size: 11px; line-height: 1.6; margin-top: 6px; padding: 6px 8px; border-radius: 6px;
  white-space: pre-wrap; word-break: break-word;
  color: #9ff0bd; background: rgba(80,200,120,0.14); border: 1px solid rgba(80,220,140,0.35);
}
.setting-test-result.bad {
  color: #ffb4b4; background: rgba(255,90,90,0.14); border-color: rgba(255,120,120,0.35);
}
.setting-switch-row { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
.setting-switch {
  flex-shrink: 0; width: 64px; padding: 4px 0; font-size: 11px; font-weight: 600;
  border-radius: 12px; cursor: pointer; color: #ccc;
  background: rgba(255,255,255,0.10); border: 1px solid rgba(255,255,255,0.22);
}
.setting-switch.on { background: rgba(80,200,120,0.28); border-color: rgba(80,220,140,0.7); color: #9ff0bd; }
.setting-switch:disabled { opacity: 0.55; cursor: default; }
.setting-switch-label { font-size: 11px; color: #9aa8bd; }
.settings-foot {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding: 8px 14px 12px; border-top: 1px solid rgba(255,255,255,0.10);
}
.settings-msg { font-size: 11px; color: #ffd54f; flex: 1; }
.settings-src { font-size: 10px; color: #7f8fa6; flex: 1; }
.settings-ver { font-size: 10px; color: #7f8fa6; font-weight: 400; }
.setting-save {
  flex-shrink: 0; background: rgba(80,150,255,0.35); border: 1px solid rgba(120,180,255,0.7);
  border-radius: 6px; color: #fff; padding: 6px 18px; font-size: 12px; cursor: pointer; font-weight: 600;
}
.setting-save:hover:not(:disabled) { background: rgba(80,150,255,0.5); }
.setting-save:disabled { opacity: 0.5; cursor: default; }

/* 私钥"已保存"提示：这是只写字段，必须显式告诉用户它还在 */
.seed-saved { color: #9ff0bd; }
.seed-saved code { background: rgba(120,200,255,0.14); color: #bfe3ff; padding: 1px 4px; border-radius: 3px; }
/* 「接口地址：xxx（默认）」那行里的小按钮：看着像链接，点一下把输入框露出来 */
.llm-link-btn {
  margin-left: 6px;
  padding: 1px 6px;
  font-size: 10px;
  color: #9fd0ff;
  background: rgba(120,200,255,0.12);
  border: 1px solid rgba(120,200,255,0.35);
  border-radius: 3px;
  cursor: pointer;
}
.llm-link-btn:hover { background: rgba(120,200,255,0.22); }
.llm-endpoint-row .llm-endpoint { word-break: break-all; }

/* ── 性格输入框 ── */
.persona-input {
  resize: vertical; min-height: 64px; max-height: 200px;
  font-size: 11.5px; line-height: 1.6; font-family: inherit;
}

/* ── 全局快捷键 ── */
.hotkey-row { display: flex; gap: 6px; align-items: center; margin-bottom: 6px; }
.hotkey-label { flex: 0 0 92px; width: 92px; font-size: 11px; color: #9fd0ff; text-align: right; }
/* 只读输入框：点它是"进入录制"，不是来打字的 —— 所以用等宽字体显示组合键 */
.hotkey-input {
  flex: 1; text-align: center; cursor: pointer;
  font-family: Consolas, "Courier New", monospace; letter-spacing: 0.5px;
}
.hotkey-input.recording { border-color: rgba(120,200,255,0.85); background: rgba(120,200,255,0.12); }
.hotkey-status { margin: 4px 0 2px; }
.hotkey-item { font-size: 10.5px; line-height: 1.6; color: #8a9ab0; word-break: break-word; }
.hotkey-item.ok { color: #9ff0bd; }
.hotkey-item.bad { color: #ffb4b4; }

/* 网址快捷键三行：名称（窄）+ 网址（宽）+ 试开按钮 */
.link-row { display: flex; gap: 6px; align-items: center; margin-bottom: 6px; }
.link-row .link-name { flex: 0 0 76px; width: 76px; text-align: center; }
.link-open {
  flex-shrink: 0; width: 28px; height: 28px; line-height: 1;
  background: rgba(80,150,255,0.22); border: 1px solid rgba(120,180,255,0.5);
  border-radius: 6px; color: #cfe4ff; font-size: 14px; cursor: pointer;
}
.link-open:hover:not(:disabled) { background: rgba(80,150,255,0.4); }
.link-open:disabled { opacity: 0.35; cursor: default; }
/* 程序位置那两行：左边是程序名的文字标签 */
.path-name {
  flex: 0 0 32px; width: 32px; text-align: right;
  font-size: 11.5px; color: #9fd0ff; font-weight: 600;
}
/* 虚线边框 = "这里可以拖东西进来"；聚焦（要打字了）时恢复实线 */
.link-row input[data-drop-target] { border-style: dashed; }
.link-row input[data-drop-target]:focus { border-style: solid; }

/* 版本更新：安装按钮独占一行（整行可点，比塞在窄行里好按） */
.update-go { width: 100%; margin-top: 6px; }

/* 更新源那一行：只读文本 —— 更新地址已随程序内置，面板不需要输入框 */
.update-source {
  flex: 1; font-size: 11px; color: #9fd0ff; line-height: 28px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}

/* ── 更新对话框 ── */
.update-panel {
  position: absolute; width: 340px;
  background: rgba(10,10,25,0.96); border: 1px solid rgba(120,200,255,0.5);
  border-radius: 10px; padding: 0; z-index: 100003; pointer-events: auto; color: #fff;
  display: flex; flex-direction: column; overflow: hidden;
  backdrop-filter: blur(12px); -webkit-backdrop-filter: blur(12px);
  box-shadow: 0 8px 28px rgba(0,0,0,0.5);
}
.update-panel .chat-header { padding: 10px 14px 6px; margin-bottom: 0; }
.update-body { flex: 1; overflow-y: auto; padding: 2px 14px 12px; }
.upd-vers { display: flex; align-items: baseline; gap: 8px; flex-wrap: wrap; font-size: 12px; margin: 6px 0 2px; }
.upd-old { color: rgba(255,255,255,0.5); }
.upd-arrow { color: #7cc4ff; }
.upd-new { color: #ffd54f; font-weight: 700; }
.upd-meta { font-size: 10.5px; color: #8a9ab0; margin-bottom: 4px; }
/* 进度条：几 MB 的包里，"还剩多少"比一句"正在下载"有用得多 */
.upd-progress { margin: 8px 0 6px; }
.upd-bar { height: 8px; background: rgba(255,255,255,0.10); border-radius: 4px; overflow: hidden; }
.upd-bar-fill { height: 100%; width: 0; background: linear-gradient(90deg, #5aa0ff, #7cc4ff); transition: width 0.2s; }
.upd-progress-text { font-size: 10.5px; color: #9fd0ff; margin-top: 4px; }
.upd-actions { display: flex; gap: 6px; margin-top: 8px; }
.upd-actions .setting-save { flex: 1; padding: 6px 0; }
.upd-skip {
  flex: 1; background: rgba(255,255,255,0.08); border: 1px solid rgba(255,255,255,0.2);
  border-radius: 6px; color: #ccc; padding: 6px 0; font-size: 11.5px; cursor: pointer;
}
.upd-skip:hover { background: rgba(255,255,255,0.16); }
/* 更新说明：保留换行、限高滚动（发布者的说明可能挺长） */
.update-notes {
  font-size: 11px; line-height: 1.6; color: rgba(255,255,255,0.78);
  white-space: pre-wrap; word-break: break-word;
  max-height: 120px; overflow-y: auto;
  background: rgba(255,255,255,0.05); border-radius: 6px;
  padding: 7px 9px; margin-top: 6px;
  scrollbar-width: thin;
}

/* 上手说明面板（环形菜单第 8 格）：尺寸与设置面板一致，好共用同一套自动摆位 */
.help-panel {
  position: absolute; width: 340px;
  background: rgba(10,10,25,0.95); border: 1px solid rgba(255,205,130,0.42);
  border-radius: 10px; padding: 0; z-index: 100002; pointer-events: auto; color: #fff;
  display: flex; flex-direction: column; overflow: hidden;
  backdrop-filter: blur(12px); -webkit-backdrop-filter: blur(12px);
  box-shadow: 0 6px 24px rgba(0,0,0,0.45);
}
.help-panel .chat-header { padding: 10px 14px 6px; margin-bottom: 0; }
.help-body {
  flex: 1; overflow-y: auto; padding: 2px 14px 12px;
  scrollbar-width: thin; scrollbar-color: rgba(255,255,255,0.2) transparent;
}
.help-body::-webkit-scrollbar { width: 4px; }
.help-body::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.2); border-radius: 2px; }
.help-sec {
  display: flex; align-items: baseline; gap: 6px;
  font-size: 12px; font-weight: 700; color: #ffd9a0;
  margin: 12px 0 6px; padding-bottom: 4px;
  border-bottom: 1px solid rgba(255,255,255,0.10);
}
.help-sec:first-child { margin-top: 8px; }
.help-hint { font-size: 10px; font-weight: 400; color: rgba(255,255,255,0.45); }
.help-row { display: flex; gap: 8px; font-size: 11.5px; line-height: 1.5; padding: 2px 0; }
/* 左列定宽右对齐：一行行读下来，动作和说明对得齐 */
.help-key { flex-shrink: 0; width: 64px; text-align: right; color: #9fd0ff; font-weight: 600; }
.help-desc { color: rgba(255,255,255,0.88); }
.help-danger .help-key { color: #ff9a9a; }
.help-danger .help-desc { color: #ffd0d0; }
.help-tip {
  font-size: 11px; line-height: 1.7; color: rgba(255,255,255,0.72);
  background: rgba(255,255,255,0.05); border-radius: 6px;
  padding: 7px 9px; margin: 5px 0 2px;
}
.help-tip b { color: #ffd9a0; }
.help-tip code {
  background: rgba(120,200,255,0.14); color: #bfe3ff;
  padding: 1px 4px; border-radius: 3px; font-size: 10.5px;
}

/* 记忆面板 */
.memory-panel {
  position: absolute; width: 360px;
  background: rgba(10,10,25,0.72); border: 1px solid rgba(180,140,255,0.35);
  border-radius: 6px; padding: 0; z-index: 100001; pointer-events: auto; color: #fff;
  display: flex; flex-direction: column; overflow: hidden;
  backdrop-filter: blur(12px); -webkit-backdrop-filter: blur(12px);
}
.memory-body {
  flex: 1; overflow-y: auto; padding: 10px 14px;
  scrollbar-width: thin; scrollbar-color: rgba(255,255,255,0.2) transparent;
}
.memory-body::-webkit-scrollbar { width: 4px; }
.memory-body::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.2); border-radius: 2px; }
.memory-section { margin-bottom: 10px; }
.memory-section-title {
  font-size: 12px; color: rgba(255,255,255,0.5); font-weight: 600;
  margin-bottom: 4px; border-bottom: 1px solid rgba(255,255,255,0.08); padding-bottom: 3px;
}
.memory-item {
  display: flex; align-items: flex-start; gap: 6px; padding: 5px 4px;
  border-radius: 6px; font-size: 13px; line-height: 1.5;
}
.memory-item.fact { background: rgba(100,180,255,0.08); }
.memory-item.preference { background: rgba(255,150,200,0.08); }
.memory-cat { flex-shrink: 0; font-size: 11px; opacity: 0.7; min-width: 60px; }
.memory-content { flex: 1; word-break: break-word; }
.memory-del {
  flex-shrink: 0; background: rgba(255,80,80,0.1); border: 1px solid rgba(255,80,80,0.15);
  border-radius: 4px; cursor: pointer; font-size: 12px; padding: 2px 6px;
  line-height: 1; color: rgba(255,150,150,0.6);
}
.memory-del:hover { background: rgba(255,80,80,0.25); color: #ff8888; border-color: rgba(255,80,80,0.3); }
.memory-msg {
  display: flex; gap: 6px; padding: 3px 4px; font-size: 12px; line-height: 1.4;
  border-radius: 4px; align-items: baseline;
}
.memory-msg.user { background: rgba(255,255,255,0.03); }
.memory-msg.assistant { background: rgba(255,180,200,0.05); }
.memory-role { flex-shrink: 0; }
.memory-msg-text { flex: 1; word-break: break-word; color: #ccc; }
.memory-time { flex-shrink: 0; font-size: 10px; color: rgba(255,255,255,0.3); }
.memory-empty, .memory-loading, .memory-error {
  text-align: center; padding: 20px 10px; font-size: 13px; color: rgba(255,255,255,0.5);
}
.memory-error { color: #ff8888; }
.memory-footer {
  display: flex; gap: 8px; justify-content: center; padding: 8px 14px;
  border-top: 1px solid rgba(255,255,255,0.1);
}
.memory-footer button {
  padding: 4px 12px; border-radius: 6px; border: 1px solid rgba(255,255,255,0.2);
  background: rgba(255,255,255,0.08); color: #ccc; font-size: 12px; cursor: pointer;
}
.memory-footer button:hover { background: rgba(255,255,255,0.15); }
.memory-clear { border-color: rgba(255,80,80,0.25) !important; color: #ff9999 !important; }
.memory-clear:hover { background: rgba(255,80,80,0.15) !important; border-color: #ff6666 !important; color: #ff6666 !important; }
.memory-toast {
  width: 100%; text-align: center; padding: 4px 8px; font-size: 12px;
  color: #aaffaa; background: rgba(100,255,100,0.08); border-radius: 6px;
  margin-bottom: 4px;
}
/* 记忆按钮：文字版（原来是 🧠 图标） */
.memory-btn { background: rgba(180,130,255,0.2) !important; color: #d4b8ff !important; font-size: 12px; }
.memory-btn:hover { background: rgba(180,130,255,0.35) !important; }

/* 右键饼状菜单 */
.pie-overlay {
  position: fixed; inset: 0; z-index: 200000; background: transparent; pointer-events: auto;
}
.pie-container {
  position: absolute; transform: translate(-50%, -50%);
  filter: drop-shadow(0 4px 24px rgba(0,0,0,0.55));
  pointer-events: auto;
}

/* 悬浮信息弹窗 */
.info-popup {
  position: absolute; min-width: 190px; max-width: 260px;
  padding: 14px 18px;
  background: rgba(255,255,255,0.95); border: 2px solid rgba(100,150,255,0.5);
  border-radius: 18px; color: #333; font-size: 13px; line-height: 1.6;
  z-index: 100002; pointer-events: auto;
  box-shadow: 0 4px 20px rgba(0,0,0,0.15);
}
.info-popup::after {
  content: ''; position: absolute; bottom: -10px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0;
  border-left: 10px solid transparent; border-right: 10px solid transparent;
  border-top: 10px solid rgba(255,255,255,0.95);
}
.info-popup::before {
  content: ''; position: absolute; bottom: -13px; left: 50%; transform: translateX(-50%);
  width: 0; height: 0;
  border-left: 11px solid transparent; border-right: 11px solid transparent;
  border-top: 11px solid rgba(100,150,255,0.5);
}
.info-date {
  font-size: 14px; font-weight: 700; color: #1a1a2e; margin-bottom: 6px;
}
.info-holiday {
  color: #e74c3c; font-weight: 600; margin-bottom: 4px; font-size: 13px;
}
.info-weather {
  display: flex; flex-wrap: wrap; align-items: center; gap: 6px;
}
.info-weather-icon { font-size: 30px; line-height: 1; }
.info-temp { font-size: 24px; font-weight: 700; color: #1a1a2e; }
.info-desc { color: #555; font-size: 13px; }
.info-details {
  width: 100%; font-size: 11px; color: #999; margin-top: 2px;
}
/* 未来 3 天预报：一行一天，窄，不抢主信息的视觉 */
.info-forecast {
  width: 100%; margin-top: 6px; padding-top: 6px;
  border-top: 1px solid rgba(0,0,0,0.08);
  display: flex; flex-direction: column; gap: 2px;
}
.info-forecast-day {
  display: flex; align-items: center; gap: 6px;
  font-size: 11px; color: #666; white-space: nowrap;
}
.info-forecast-label { width: 24px; color: #999; flex-shrink: 0; }
.info-forecast-icon { font-size: 13px; line-height: 1; }
.info-forecast-temp { width: 62px; flex-shrink: 0; }
.info-forecast-desc { color: #999; }
.info-location-row { width: 100%; margin-top: 6px; }
.info-location-label {
  font-size: 11px; color: #aaa; cursor: pointer; user-select: none;
}
.info-location-label:hover { color: #6688cc; }
.info-location-edit { font-size: 10px; opacity: 0; transition: opacity 0.15s; }
.info-location-label:hover .info-location-edit { opacity: 1; }
.info-location-input {
  width: 100%; padding: 2px 6px; border-radius: 4px;
  border: 1px solid rgba(100,150,255,0.4); background: rgba(255,255,255,0.9);
  font-size: 12px; color: #333; outline: none;
}
.info-location-input:focus { border-color: rgba(100,150,255,0.8); }
.info-weather-loading, .info-weather-error, .info-weather-empty {
  font-size: 12px; color: #999; font-style: italic;
}
.info-weather-error { color: #e74c3c; }
</style>
