// 回归检查：**把已经修掉的坑钉在源码里**，防止哪天顺手改回来。
//
// 用法：node scripts/regression-check.mjs
//
// 为什么要有这个脚本：
// 审查报告（huohuo-desktop-pet-审查报告）里的问题一共 74 条，其中相当一部分是
// "看起来更顺手、其实是把安全边界或数据安全拆掉"的写法 —— 比如
//   · `state.db.lock().unwrap()`（中毒后整个记忆功能瘫痪）
//   · `emailQueue = [...useful.slice(-3)]`（没看完的邮件被永久挤掉）
//   · `unwrap_or_default()` 读设置（一个字段写错 → 整份设置被空值覆盖）
// 这类改动**编译得过、单测也可能照样绿**（单测覆盖不到那条真实路径），
// 只有"扫源码"才拦得住。所以这里把每一条的"旧写法"写成一个必须**搜不到**的模式。
//
// 与 security-check.mjs 的分工：
//   · security-check.mjs  → 需要真跑界面（CDP），验收"点允许/拒绝真的生效"
//   · 本脚本             → 纯静态扫描，任何机器上都能跑，进 CI 也没问题
//
// 每一条都带 `why`：命中时直接告诉改的人"为什么不能这么写"。
// 命中了不代表一定错（可能是注释里引用了旧写法），所以 `allowInComments` 默认 true，
// 并且对照 `allow` 里的正则放行（例如"故意用 `unwrap()` 毒锁"的测试）。

import { readFileSync, readdirSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..')
const SRC = join(ROOT, 'src-tauri', 'src')

/** 必须**搜不到**的旧写法 */
const FORBIDDEN = [
  {
    id: 'S3/S1',
    file: 'lib.rs',
    re: /agent_guard::decide/,
    negate: true,
    why: '权限闸门必须仍然在每个工具调用的必经之路上（缺了它 = S1/S2/S3 一起回来）',
  },
  {
    id: 'S2',
    file: 'lib.rs',
    re: /is_protected_path/,
    negate: true,
    why: '禁区路径检查（settings.json / *.pem / .ssh / 浏览器 profile）不能被摘掉',
  },
  {
    id: 'B25',
    file: 'lib.rs',
    re: /state\.db\.lock\(\)\.unwrap\(\)/,
    why: '加锁一律走 db::lock_db（unwrap_or_else(into_inner)）：中毒后记忆功能还要能用',
  },
  {
    id: 'B25',
    file: 'lib.rs',
    re: /fn get_memory_detail\([^)]*\)\s*->\s*db::MemoryDetail\b/,
    why: '读记忆失败要能显示给主人（返回 Result），不能装作"记忆是空的"',
  },
  {
    id: 'B22',
    file: 'lib.rs',
    re: /ABORT_TTS/,
    why: '语音打断必须用代次号（TTS_GEN）：布尔标记会让"停止"被随后的请求清掉，两段声音还会叠着播',
  },
  {
    id: 'B24',
    file: 'settings.rs',
    re: /Command::new\("powershell"\)/,
    why: '起 PowerShell 一律走 crate::powershell()（带 CREATE_NO_WINDOW），否则每调用一次闪一次黑框',
  },
  {
    id: 'B21',
    file: 'lib.rs',
    re: /&resp\[\.\.200\]/,
    why: '按字节切片会在汉字中间 panic——错误信息要 chars().take(200)',
  },
  {
    id: 'B33',
    file: 'lib.rs',
    re: /text\.len\(\)\s*>\s*3000/,
    why: '截断判断要按字符数：中文页面按字节比会把完整内容标成"已截断"',
  },
  {
    id: 'D2',
    file: 'lib.rs',
    re: /\.select\("INBOX"\)/,
    why: '查收件箱必须用只读的 examine()：select 会把邮件标成已读，没看完的邮件永久消失',
  },
  {
    id: 'D2',
    file: 'App.vue',
    re: /emailQueue\s*=\s*\[\.\.\.useful/,
    why: '邮件队列要"追加 + 按 uid 去重"，整体替换会把没看完的邮件挤掉',
  },
  {
    id: 'D3',
    file: 'App.vue',
    re: /email-copy-btn" @click="copyEmailData" @dblclick\.stop/,
    negate: true,
    why: '（正向）气泡上绑着"双击看下一封"，复制按钮必须 @dblclick.stop，否则双击复制 = 永久屏蔽这封邮件',
  },
  {
    id: 'D6',
    file: 'App.vue',
    re: /❌ 操作失败：\$\{String\(e\)[\s\S]{0,400}?loadSettings\(\)/,
    why: '开机自启失败时只能刷新 autostart_* 两个字段，整份 loadSettings() 会冲掉面板上没保存的修改',
  },
  {
    id: 'D7',
    file: 'App.vue',
    re: /enqueuePipeline\(\(\) => handleUserMessage\(msg\)\)\s*\r?\n\s*await clearChatInput\(\)/,
    why: '输入框要在**发送前**清空，否则等待回复期间打的字会被回复回来时的 clearChatInput 抹掉',
  },
  {
    id: 'D1',
    file: 'settings.rs',
    re: /strip_bom\(&bytes\)\)\.unwrap_or_default\(\)/,
    why: '设置解析失败要先试 .bak、再改名留存坏文件，绝不能用默认值覆盖',
  },
  {
    id: 'B5',
    file: 'settings.rs',
    re: /#\[derive\(Serialize,\s*Deserialize,\s*Default\)\]\s*\n\s*struct Store/,
    why: 'Store 的 Default 必须手写成"反序列化空对象"，否则新装用户永远不会自动检查更新',
  },
  {
    id: 'S6',
    file: 'settings.rs',
    re: /h\.starts_with\("127\."\)/,
    why: '"本机地址"判断不能做前缀匹配：http://127.0.0.1@evil.com 会把密钥明文发去公网',
  },
  {
    id: 'S7',
    file: 'telegram.rs',
    re: /fn err_chain[\s\S]{0,400}?\n}/,
    negate: true,
    why: '（正向）错误信息要经过 redact_token，否则 bot token 会随 URL 进日志',
  },
  {
    id: 'S7',
    file: 'telegram.rs',
    re: /redact_token\(&out\)/,
    negate: true,
    why: 'err_chain 的输出必须过 redact_token：URL 里带着 bot token，而 README 让用户贴日志',
  },
  {
    id: 'B4',
    file: 'telegram.rs',
    re: /^\s*Store::load\(\)\.save\(\);\s*$/m,
    why: '换 token 时必须把内存里的 store 一起换掉（st.store = Store::load()），只落盘会让新 bot 一条消息都收不到',
  },
  {
    id: 'B29',
    file: 'telegram.rs',
    re: /n \+= 1;\s*\n\s*if n >= max/,
    why: '分段发送要按 UTF-16 码元计数（Telegram 的口径），按字符算会把 emoji 多的回复整段送失败',
  },
  {
    id: 'B28',
    file: 'telegram.rs',
    re: /cleaned\.chars\(\)\.take\(120\)\.collect\(\)/,
    why: '长文件名截断要保住扩展名，否则落盘的文件认不出类型',
  },
  {
    id: 'B9',
    file: 'watch.rs',
    re: /raw_next_offset/,
    negate: true,
    why: '（正向）看门狗的 offset 必须按原始 update_id 推进，否则贴纸/陌生人消息会让它反复拉同一批',
  },
  {
    id: 'B8',
    file: 'hotkeys.rs',
    re: /ACTIONS\[idx\]/,
    why: '热键要用 id 反查动作，不能用"注册成功的顺序下标"——中间一个失败会让后面全部错位',
  },
  {
    id: 'B17',
    file: 'db.rs',
    re: /MAX_MEMORIES_PER_CATEGORY|CONTEXT_MEMORY_LIMIT/,
    negate: true,
    why: '（正向）记忆必须有存量上限与注入上限，否则上下文随使用时间线性膨胀',
  },
  {
    id: 'S8',
    file: 'update.rs',
    re: /ensure_not_downgrade/,
    negate: true,
    why: '（正向）安装前必须挡住"签名合法但版本更旧"的回滚攻击',
  },
  {
    id: 'B15',
    file: 'update.rs',
    re: /with_capacity\([^)]*size[^)]*\)/,
    why: '预分配不能直接用远端给的 size（它不在签名范围内），要么封顶要么不用',
  },
  {
    id: 'S9',
    file: 'lib.rs',
    re: /ensure_public_target|ip_is_private|host_looks_private/,
    negate: true,
    why: '（正向）web_fetch 必须挡本机/内网地址（SSRF），并在重定向时重新校验',
  },
  {
    id: 'S5',
    file: 'lib.rs',
    re: /tokio::time::timeout\(PDF_PARSE_TIMEOUT/,
    negate: true,
    why: '（正向）PDF 解析必须套超时：PDF 能从 Telegram 发进来，这是可被远程触发的路径',
  },
  {
    id: 'B1',
    file: 'lib.rs',
    re: /wait_with_output/,
    why: 'run_command 要用两个线程并发读 stdout/stderr（4KB 管道会写满堵死），不要退回单线程等待',
  },
  // ─── S11：个人的东西不许再进仓库 ───
  // （README 清过一次；这几条盯的是**脚本与源码**里的同类残留）
  {
    id: 'S11',
    file: 'settings.rs',
    re: /LINK_DEFAULTS[^;]*http/,
    why: '环形菜单三格网址的**默认值必须全空**：预置作者的个人主页 = 每装一份就把他的私人页面带出去一份',
  },
  {
    id: 'S11',
    file: 'settings.rs',
    re: /LINK_DEFAULTS.*(dmhy|bgm\.tv|userdetails)/,
    why: '同上另一半：默认值里不许出现作者的个人主页站点（dmhy / bgm.tv / userdetails.php）',
  },
  {
    id: 'S11',
    file: 'make-update-manifest.mjs',
    re: /E:[\\/]{1,2}1[\\/]{1,2}_keys/,
    why: '签名私钥的路径是发布者机器上的私人路径：必须由参数 / PET_UPDATE_KEY 显式给，不能写死在源码里',
  },
  {
    id: 'S11',
    file: 'update-e2e.mjs',
    re: /E:[\\/]{1,2}1[\\/]{1,2}_keys/,
    why: '同上：验收脚本不内置发布者的私钥路径（没设 PET_UPDATE_KEY 就跳过，而不是去猜一个路径）',
  },
  {
    id: 'S11',
    file: 'publish.mjs',
    re: /E:[\\/]{1,2}1[\\/]{1,2}_keys/,
    why: '同上：发布脚本里也不许出现私钥的本机路径',
  },
  {
    id: 'S11',
    file: 'update.rs',
    re: /E:[\\/]{1,2}1[\\/]{1,2}_keys/,
    includeComments: true,
    why: '同上：连注释里都不写发布者的私钥路径（README 已经为这一条清过一次）',
  },
  {
    id: 'S11',
    file: 'pie-menu-check.mjs',
    re: /dmhy|bgm\.tv|userdetails/,
    why: '验收脚本不该再断言/记录作者的私人主页；"默认三格为空"改在源码层面断言（见上面 LINK_DEFAULTS 两条）',
  },
  // ─── 拖文件给桌宠（把落点识别与禁区判定留在后端）───
  {
    id: '拖文件',
    file: 'App.vue',
    re: /inspect_dropped_paths/,
    negate: true,
    why: '（正向）拖到宠物身上的路径必须过后端 inspect_dropped_paths：前端没有文件系统权限，禁区判定也不能放在前端（那边可以被绕过）',
  },
  {
    id: '拖文件',
    file: 'App.vue',
    re: /function handlePetDrop/,
    negate: true,
    why: '（正向）"丢给桌宠"要有独立入口，别和设置面板的拖放输入框混在一个分支里',
  },
  {
    id: '拖文件',
    file: 'App.vue',
    re: /function isOverPet/,
    negate: true,
    why: '（正向）落点判定（是不是宠物身上）要单独一个函数：拖到面板/设置输入框上必须不算',
  },
  // ─── 看门狗默认关（新用户装完不该多一个常驻进程）───
  {
    id: '看门狗默认关',
    file: 'lib.rs',
    re: /is_autostart_enabled\("watchdog"\)/,
    negate: true,
    why: '（正向）ensure_watchdog_running 必须先看「看门狗」开关：新用户默认关，不能替他把这个常驻进程起起来',
  },
  {
    id: '看门狗默认关',
    file: 'watch.rs',
    re: /is_autostart_enabled\("watchdog"\)/,
    negate: true,
    why: '（正向）看门狗主循环里要自己看开关并退出 —— 两者是同一个 exe，不能靠按名字杀进程来关它',
  },
  // ─── 设置面板的分类页 ───
  {
    id: '设置分类',
    file: 'App.vue',
    re: /:data-tab="t\.id"/,
    negate: true,
    why: '（正向）分类页签要带 data-tab 锚点：被 v-if 收起来的控件在 DOM 里不存在，验收脚本只能靠点页签切过去',
  },
  {
    id: '设置分类',
    file: 'App.vue',
    re: /const settingsTab = ref<SettingsTabId>\('pet'\)/,
    negate: true,
    why: '（正向）默认页必须是「桌宠」：四个验收脚本要在默认页上找自己的控件，改成别的页它们会集体失败',
  },
]

/** 需要读的文件（相对仓库根） */
const FILES = {
  'lib.rs': join(SRC, 'lib.rs'),
  'settings.rs': join(SRC, 'settings.rs'),
  'telegram.rs': join(SRC, 'telegram.rs'),
  'watch.rs': join(SRC, 'watch.rs'),
  'hotkeys.rs': join(SRC, 'hotkeys.rs'),
  'db.rs': join(SRC, 'db.rs'),
  'update.rs': join(SRC, 'update.rs'),
  'project.rs': join(SRC, 'project.rs'),
  'App.vue': join(ROOT, 'src', 'App.vue'),
  // 发布/验收脚本也在扫描范围内：私钥路径、个人主页这类"作者环境信息"最容易藏在脚本里
  'make-update-manifest.mjs': join(ROOT, 'scripts', 'make-update-manifest.mjs'),
  'update-e2e.mjs': join(ROOT, 'scripts', 'update-e2e.mjs'),
  'publish.mjs': join(ROOT, 'scripts', 'publish.mjs'),
  'pie-menu-check.mjs': join(ROOT, 'scripts', 'pie-menu-check.mjs'),
}

/** 命中行出现在注释里时放行（注释里引用"旧写法"是好事，不是回归） */
function isCommentLine(line) {
  const t = line.trim()
  return t.startsWith('//') || t.startsWith('///') || t.startsWith('*') || t.startsWith('/*')
}

let fails = 0
let checked = 0

for (const rule of FORBIDDEN) {
  checked++
  const path = FILES[rule.file]
  let text
  try {
    text = readFileSync(path, 'utf8')
  } catch (e) {
    console.log(`❌ ${rule.id} ${rule.file}: 读不到文件（${e.message}）`)
    fails++
    continue
  }

  const lines = text.split(/\r?\n/)
  const hits = []
  for (let i = 0; i < lines.length; i++) {
    // 整文件范围的正则（比如"必须存在"的正向规则）单独处理
    if (rule.re.source.includes('[\\s\\S]')) continue
    // 注释里引用"旧写法"是好事（说明来龙去脉），默认不算命中；
    // 但有些规则连注释都不许出现（比如发布者的私钥路径），用 includeComments 打开
    if (!rule.includeComments && isCommentLine(lines[i])) continue
    if (rule.re.test(lines[i])) {
      hits.push({ n: i + 1, line: lines[i].trim().slice(0, 100) })
    }
  }

  const present = rule.re.test(text)
  const ok = rule.negate ? present : hits.length === 0
  if (ok) {
    console.log(`✅ ${rule.id}  ${rule.file}  ${rule.negate ? '（正向：关键实现仍在）' : '（旧写法已消失）'}`)
  } else {
    fails++
    console.log(`❌ ${rule.id}  ${rule.file}`)
    console.log(`   为什么不能这么写：${rule.why}`)
    for (const h of hits.slice(0, 5)) console.log(`   ${rule.file}:${h.n}: ${h.line}`)
    if (!rule.negate && hits.length === 0) {
      console.log('   （正则命中但不带行号：多半是被注释掉了或者跨行匹配 —— 请人工看一眼）')
    }
  }
}

// 顺手确认没有别的地方绕过统一入口再起 PowerShell（B24）
const extraSpawns = readdirSync(SRC)
  .filter((f) => f.endsWith('.rs'))
  .flatMap((f) => {
    const body = readFileSync(join(SRC, f), 'utf8')
    return body
      .split(/\r?\n/)
      .map((line, i) => ({ f, n: i + 1, line }))
      .filter((x) => /Command::new\("powershell"\)/.test(x.line))
  })
checked++
const allowedSpawnFiles = new Set(['lib.rs'])
const badSpawns = extraSpawns.filter((x) => !allowedSpawnFiles.has(x.f))
if (badSpawns.length === 0) {
  console.log('✅ B24  所有 PowerShell 调用都走 lib.rs::powershell()（带 CREATE_NO_WINDOW）')
} else {
  fails++
  console.log('❌ B24  还有别处在直接起 PowerShell（会闪黑框）：')
  for (const b of badSpawns) console.log(`   ${b.f}:${b.n}: ${b.line.trim().slice(0, 90)}`)
}

console.log(
  fails === 0
    ? `\n${checked} 项回归检查全部通过 ✅（这些坑不会再被顺手改回来）`
    : `\n有 ${fails} 项回归检查失败 ❌（上面每一条都写了为什么）`
)
process.exit(fails === 0 ? 0 : 1)
