//! 零新增依赖的 PDF 正文文本抽取（只用 `std` + `flate2`）。
//!
//! 对外只暴露一个函数：
//!
//! ```ignore
//! let text = pdf::extract_pdf_text(std::path::Path::new("a.pdf"))?;
//! ```
//!
//! 实现范围：
//! 1. 全文件扫描 `N G obj ... endobj`，取 `stream` / `endstream` 之间的原始字节；
//! 2. `/FlateDecode`（zlib，`ZlibDecoder`）+ ASCIIHex/ASCII85/RunLength，PNG/TIFF 预测器；
//! 3. 内容流文本算子 `Tj` `TJ` `'` `"`，PDF 字符串转义与十六进制串，
//!    `Td` `TD` `T*` `ET` 触发换行，`/F1 12 Tf` 跟踪当前字体，`Do` 递归 Form XObject；
//! 4. `/ToUnicode` CMap：`beginbfchar` / `beginbfrange`（含数组形式）/ `begincodespacerange`，
//!    目标串按 UTF-16BE 解码（支持代理对）；
//! 5. 多页：走 Catalog → Pages → Kids 树，退化时按 `/Type /Page`，再退化时按内容流顺序；
//! 6. 稳健性：不 panic、大小/时间/页数上限、结果为空时返回中文错误。
//!
//! 已知局限见文件末尾 `mod tests` 上方注释与交付说明。

use flate2::read::{DeflateDecoder, ZlibDecoder};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

// ───────────────────────── 上限保护 ─────────────────────────

/// 只读文件前 200 MB。
const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
/// 最多处理 500 页。
const MAX_PAGES: usize = 500;
/// 单个流解压后的字节上限（防 zlib 炸弹）。
const MAX_STREAM_OUT: usize = 64 * 1024 * 1024;
/// 单个流的解压时间上限。
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);
/// 整个解析过程的时间上限。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
/// Form XObject 递归深度上限。
const MAX_FORM_DEPTH: usize = 8;
/// 单个 CMap 最多收录多少条映射。
const MAX_CMAP_ENTRIES: usize = 300_000;
/// 单次词法分析最多产出多少 token。
const MAX_LEX_TOKENS: usize = 4_000_000;
/// 最多解析多少个间接对象。
const MAX_OBJECTS: usize = 200_000;
/// 字典 / 数组的最大嵌套深度。
const MAX_NEST_DEPTH: usize = 64;
/// 索引对象时，字典区域的兜底扫描长度。
const MAX_DICT_SCAN: usize = 1024 * 1024;
/// 页面树 / `/Parent` 父链的最大深度。
///
/// 正常 PDF 的页面树是平衡树，500 页也就十来层，32 是给畸形文件留的余量。
/// 光靠深度不够：`Kids [2 0 R 2 0 R 2 0 R 2 0 R]` 自我引用时，深度 32 下有约 4^32 次调用，
/// 线程会被永久占满 CPU，所以还要 visited 集合和总次数上限一起拦。
const MAX_TREE_DEPTH: usize = 32;
/// 遍历页面树的总访问次数上限（含重复访问）。
///
/// 正常文件：500 页的页面树最多约 1000 个节点；攻击文件想的正是靠自引用把它变成 4^33。
const MAX_TREE_VISITS: usize = 50_000;
/// 一页里 Form XObject 的展开总次数上限。
///
/// 正常文件：一页几十个 XObject 就算多了（水印、图元、表单），上千极罕见；
/// 攻击文件让一个表单里放 50 次 `/Do` 再自我嵌套，深度 8 就是 50^8 次，必须拦住。
const MAX_XOBJECT_CALLS: usize = 10_000;
/// 一页 `/Contents` 拼接后的总字节上限。
///
/// 正常文件：一页内容流几 KB ～ 几 MB（矢量图密集的页面也就几十 MB）；
/// 攻击文件会把一个解压后 63 MB 的压缩炸弹在 `/Contents` 数组里引用 500 次 ≈ 32 GB。
const MAX_CONTENT_BYTES: usize = 32 * 1024 * 1024;
/// 全文档「解压后」的总字节预算。
///
/// 正常文件：扫描件的图片流根本不解压（见 `should_decode_stream`），剩下的正文 / 字体 /
/// 对象流加起来通常几 MB ～ 几十 MB；攻击文件放几百个小压缩炸弹、每个解压出 64 MB，
/// 几十 GB 会把整个进程 OOM —— 那种死法由系统直接终止，连日志都留不下。
const MAX_TOTAL_DECOMPRESSED_BYTES: usize = 128 * 1024 * 1024;
/// 扫描单个对象体的最大推进距离。
///
/// 正常文件的字典只有几 KB（就算带一大坨字体 / 资源数组也就几十 KB），256 KB 已经很宽松；
/// 畸形文件放几千个「不写 `endobj`」的对象，旧代码每次都从对象头一路扫到文件尾，
/// n 个对象就是 n² —— 200 MB 的文件能稳定耗满 60 秒超时。
const MAX_OBJ_BODY_SCAN: usize = 256 * 1024;
/// 缺 `/Length` 时，单次向后找 `endstream` 的最大扫描距离。
///
/// 有 `/Length` 的流走快路径（校验一次就取数据），只有畸形或很老的文件才退化成向后搜索；
/// 64 MB 已经等于单流解压上限（`MAX_STREAM_OUT`），比这更远的 `endstream` 一定是在拖时间。
const MAX_STREAM_SCAN: usize = 64 * 1024 * 1024;
/// 「找不到 `endstream`」的累计扫描预算（跨整个文件，按实际白扫的字节数扣）。
///
/// 正常 PDF 几乎都写 `/Length`，走不到这条退化路径；真走到时，`endstream` 就在数据后面。
/// 攻击文件会放几千个「有 stream、没 endstream」的对象，每个都扫到文件尾 —— O(n²)：
/// 4 MB 的攻击文件就能扫出 4 GB 的搜索量。成功的搜索不记账（它会把位置推过 endstream，
/// 天然是线性的），所以这里只需要给「白扫」封顶：8 MB 够正常文件（无 /Length 的流总量）
/// 用，超出的部分按损坏流丢弃并记日志 —— 不能为了这种畸形文件把 CPU 交出去。
const MAX_STREAM_SEARCH_TOTAL: usize = 8 * 1024 * 1024;

const NO_TEXT_MSG: &str = "PDF 里没抽到文字（可能是扫描件图片，或用了无法还原的字体编码）";

// ───────────────────────── 上限告警 ─────────────────────────

/// 触到上限时留痕。
///
/// 项目规矩是「失败必须看得见」：不许在深处静默 `return`，所以每一处上限都要记一笔。
/// 直接复用 telegram 的日志（`%APPDATA%\desktop-pet\telegram.log`），出问题时贴日志就能看到。
fn note_limit(msg: &str) {
    crate::telegram::log_line(&format!("[pdf] {}", msg));
}

/// 同一类上限只记一行日志。
///
/// 畸形文件会成千上万次撞到同一处上限：每次都写盘既慢，又会把 telegram.log
/// （超过 256 KB 会被整个删掉）刷爆，把真正有用的那几行挤掉。
#[derive(Default)]
struct LimitLog {
    warned: HashSet<&'static str>,
}

impl LimitLog {
    /// `key` 相同的告警只写一次；`key` 用常量字符串，便于一眼看出是哪种上限。
    fn hit(&mut self, key: &'static str, msg: &str) {
        if self.warned.insert(key) {
            note_limit(msg);
        }
    }
}

// ───────────────────────── 对外 API ─────────────────────────

/// 抽取 PDF 的正文文本。失败时返回中文错误说明。
///
/// 不会 panic：所有畸形输入都走 `Err` 或“跳过该对象”。
pub fn extract_pdf_text(path: &Path) -> Result<String, String> {
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let data = read_pdf_bytes(path)?;
    if !looks_like_pdf(&data) {
        return Err("这不是有效的 PDF 文件：未找到 %PDF- 文件头（可能是其它格式或文件已损坏）".to_string());
    }

    let doc = Doc::build(&data, deadline);
    let mut lim = LimitLog::default();
    let pages = doc.page_order(&mut lim);
    let mut cmaps: HashMap<u32, Rc<FontInfo>> = HashMap::new();
    let mut out = String::new();

    if pages.is_empty() {
        // 退化路径：没有可用的页面树时，按文件顺序把所有“像内容流”的流当正文。
        for num in &doc.order {
            if Instant::now() > deadline {
                lim.hit("fallback_deadline", "退化路径超过总时间上限，剩余流已放弃");
                break;
            }
            if !doc.stream_is_text_candidate(*num) {
                continue;
            }
            let bytes = match doc.stream(*num) {
                Some(b) => b.to_vec(),
                None => continue,
            };
            if !looks_like_content(&bytes) {
                continue;
            }
            let res = Val::Dict(Vec::new());
            let fonts = doc.build_font_map(&res, &mut cmaps);
            let mut ctx = Ctx::new(&doc, deadline);
            // 退化路径下每个流当一页看，所以 guard（Form XObject 预算）也按流重置。
            let mut guard = RenderGuard::new();
            render_content(&mut ctx, &mut guard, &mut cmaps, &bytes, &fonts, &res, 0);
            let part = sanitize(&ctx.out);
            if !part.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&part);
            }
        }
    } else {
        for num in pages {
            if Instant::now() > deadline {
                lim.hit("pages_deadline", "渲染页面超过总时间上限，剩余页面已放弃");
                break;
            }
            let part = sanitize(&doc.render_page(num, &mut cmaps, deadline, &mut lim));
            if !part.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&part);
            }
        }
    }

    if out.is_empty() {
        if find_sub(&data, b"/Encrypt").is_some() {
            return Err("PDF 已加密（含 /Encrypt），本模块不支持解密，无法抽取文字".to_string());
        }
        return Err(NO_TEXT_MSG.to_string());
    }
    Ok(out)
}

// ───────────────────────── 读文件 ─────────────────────────

fn read_pdf_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Err(format!("读取 PDF 失败：{}（{}）", path.display(), e)),
    };
    if !meta.is_file() {
        return Err(format!("不是文件：{}", path.display()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "PDF 太大（{} 字节，上限 {} 字节 = 200 MB），已放弃解析",
            meta.len(),
            MAX_FILE_BYTES
        ));
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(format!("读取 PDF 失败：{}（{}）", path.display(), e)),
    };
    let cap = if meta.len() > 1024 * 1024 { 1024 * 1024 } else { meta.len() } as usize;
    let mut buf = Vec::with_capacity(cap);
    let mut limited = file.take(MAX_FILE_BYTES);
    if let Err(e) = limited.read_to_end(&mut buf) {
        return Err(format!("读取 PDF 失败：{}（{}）", path.display(), e));
    }
    Ok(buf)
}

fn looks_like_pdf(data: &[u8]) -> bool {
    let n = if data.len() < 1024 { data.len() } else { 1024 };
    find_sub(&data[..n], b"%PDF-").is_some()
}

// ───────────────────────── 字节小工具 ─────────────────────────

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delim(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_token_end(b: u8) -> bool {
    is_ws(b) || is_delim(b)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 查找关键字 `kw`，要求两侧都是 token 边界（避免命中 `endstream` 里的 `stream`）。
fn find_keyword(hay: &[u8], from: usize, kw: &[u8]) -> Option<usize> {
    if kw.is_empty() || from >= hay.len() {
        return None;
    }
    let mut i = from;
    while i + kw.len() <= hay.len() {
        let rel = match find_sub(&hay[i..], kw) {
            Some(r) => r,
            None => return None,
        };
        let at = i + rel;
        let before_ok = at == 0 || is_token_end(hay[at - 1]);
        let after = at + kw.len();
        let after_ok = after >= hay.len() || is_token_end(hay[after]);
        if before_ok && after_ok {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

// ───────────────────────── 值模型 ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Array(Vec<Val>),
    Dict(Vec<(String, Val)>),
    Ref(u32, u16),
    /// 裸关键字（`obj` `endobj` `Tj` `R` …）
    Op(String),
}

fn v_get<'a>(v: &'a Val, key: &[u8]) -> Option<&'a Val> {
    if let Val::Dict(entries) = v {
        for (k, val) in entries {
            if k.as_bytes() == key {
                return Some(val);
            }
        }
    }
    None
}

fn v_name(v: &Val) -> Option<&str> {
    match v {
        Val::Name(s) => Some(s.as_str()),
        _ => None,
    }
}

fn v_ref(v: &Val) -> Option<u32> {
    match v {
        Val::Ref(n, _) => Some(*n),
        _ => None,
    }
}

fn v_int(v: &Val) -> Option<i64> {
    match v {
        Val::Int(i) => Some(*i),
        Val::Real(r) => Some(*r as i64),
        _ => None,
    }
}

fn v_str(v: &Val) -> Option<&[u8]> {
    match v {
        Val::Str(s) => Some(s.as_slice()),
        _ => None,
    }
}

fn v_name_or<'a>(v: Option<&'a Val>, dflt: &'a str) -> &'a str {
    match v {
        Some(x) => match v_name(x) {
            Some(s) => s,
            None => dflt,
        },
        None => dflt,
    }
}

// ───────────────────────── 词法分析 ─────────────────────────

struct Lexer<'a> {
    d: &'a [u8],
    i: usize,
    allow_refs: bool,
    skip_inline_images: bool,
}

impl<'a> Lexer<'a> {
    fn skip_ws(&mut self) {
        let n = self.d.len();
        loop {
            while self.i < n && is_ws(self.d[self.i]) {
                self.i += 1;
            }
            if self.i < n && self.d[self.i] == b'%' {
                while self.i < n && self.d[self.i] != b'\n' && self.d[self.i] != b'\r' {
                    self.i += 1;
                }
                continue;
            }
            return;
        }
    }
}

/// 通用词法分析：把一段字节变成 `Val` 序列。
fn lex(data: &[u8], allow_refs: bool, skip_inline_images: bool) -> Vec<Val> {
    let mut p = Lexer {
        d: data,
        i: 0,
        allow_refs,
        skip_inline_images,
    };
    let mut out: Vec<Val> = Vec::new();
    while p.i < p.d.len() && out.len() < MAX_LEX_TOKENS {
        let before = p.i;
        let item = lex_one(&mut p, 0);
        match item {
            Some(v) => out.push(v),
            None => {
                if p.i <= before {
                    p.i = before.saturating_add(1);
                }
            }
        }
    }
    out
}

/// 读一个 token；`None` 表示“没有产出值”（空白、注释、不可识别字节等）。
fn lex_one(p: &mut Lexer, depth: usize) -> Option<Val> {
    if depth > MAX_NEST_DEPTH {
        return None;
    }
    p.skip_ws();
    let n = p.d.len();
    if p.i >= n {
        return None;
    }
    let b = p.d[p.i];

    // 字面字符串
    if b == b'(' {
        let (s, ni) = read_literal_string(p.d, p.i);
        p.i = ni;
        return Some(Val::Str(s));
    }

    // 字典 或 十六进制串
    if b == b'<' {
        if p.i + 1 < n && p.d[p.i + 1] == b'<' {
            p.i += 2;
            let mut entries: Vec<(String, Val)> = Vec::new();
            loop {
                p.skip_ws();
                if p.i >= n {
                    break;
                }
                if p.i + 1 < n && p.d[p.i] == b'>' && p.d[p.i + 1] == b'>' {
                    p.i += 2;
                    break;
                }
                if p.d[p.i] != b'/' {
                    let before = p.i;
                    let _ = lex_one(p, depth + 1);
                    if p.i <= before {
                        p.i = before + 1;
                    }
                    continue;
                }
                let (key, ki) = read_name(p.d, p.i);
                p.i = ki;
                p.skip_ws();
                if p.i >= n {
                    entries.push((key, Val::Null));
                    break;
                }
                if p.i + 1 < n && p.d[p.i] == b'>' && p.d[p.i + 1] == b'>' {
                    p.i += 2;
                    entries.push((key, Val::Null));
                    break;
                }
                let val = match lex_one(p, depth + 1) {
                    Some(v) => v,
                    None => Val::Null,
                };
                entries.push((key, val));
                if entries.len() >= 100_000 {
                    break;
                }
            }
            return Some(Val::Dict(entries));
        }
        let (s, ni) = read_hex_string(p.d, p.i);
        p.i = ni;
        return Some(Val::Str(s));
    }

    // 数组
    if b == b'[' {
        p.i += 1;
        let mut items: Vec<Val> = Vec::new();
        loop {
            p.skip_ws();
            if p.i >= n {
                break;
            }
            if p.d[p.i] == b']' {
                p.i += 1;
                break;
            }
            let before = p.i;
            let item = lex_one(p, depth + 1);
            if let Some(v) = item {
                items.push(v);
            } else if p.i <= before {
                p.i = before + 1;
            }
            if items.len() >= 100_000 {
                break;
            }
        }
        return Some(Val::Array(items));
    }

    // 收尾符号
    if b == b']' || b == b'}' || b == b'{' {
        p.i += 1;
        return None;
    }
    if b == b'>' {
        if p.i + 1 < n && p.d[p.i + 1] == b'>' {
            p.i += 2;
        } else {
            p.i += 1;
        }
        return None;
    }

    // 名字
    if b == b'/' {
        let (name, ni) = read_name(p.d, p.i);
        p.i = ni;
        return Some(Val::Name(name));
    }

    // 单字符算子
    if b == b'\'' || b == b'"' {
        p.i += 1;
        return Some(Val::Op((b as char).to_string()));
    }

    // 数字（可能带 `N G R` 引用）
    if b.is_ascii_digit() || b == b'+' || b == b'-' || b == b'.' {
        let (v, ni) = read_number_val(p.d, p.i, p.allow_refs);
        p.i = ni;
        return Some(v);
    }

    // 裸关键字 / 算子
    let start = p.i;
    while p.i < n && !is_token_end(p.d[p.i]) {
        p.i += 1;
    }
    if p.i == start {
        p.i += 1;
        return None;
    }
    let kw = String::from_utf8_lossy(&p.d[start..p.i]).into_owned();
    if kw == "true" {
        return Some(Val::Bool(true));
    }
    if kw == "false" {
        return Some(Val::Bool(false));
    }
    if kw == "null" {
        return Some(Val::Null);
    }
    if kw == "BI" && p.skip_inline_images {
        // 内联图片的二进制数据里可能含括号/算子，直接跳到 EI。
        if let Some(e) = find_keyword(p.d, p.i, b"EI") {
            p.i = e + 2;
        }
    }
    Some(Val::Op(kw))
}

fn read_name(d: &[u8], start: usize) -> (String, usize) {
    let n = d.len();
    let mut i = start + 1;
    let mut out: Vec<u8> = Vec::new();
    while i < n && !is_token_end(d[i]) {
        if d[i] == b'#' && i + 2 < n {
            let h1 = hex_val(d[i + 1]);
            let h2 = hex_val(d[i + 2]);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(d[i]);
        i += 1;
    }
    (String::from_utf8_lossy(&out).into_owned(), i)
}

/// 解析字面字符串（`(...)`），处理转义与括号嵌套。返回（反义后的字节, 新位置）。
fn read_literal_string(d: &[u8], start: usize) -> (Vec<u8>, usize) {
    let n = d.len();
    let mut out: Vec<u8> = Vec::new();
    let mut i = start;
    if i >= n || d[i] != b'(' {
        return (out, i);
    }
    i += 1;
    let mut depth: i32 = 1;
    while i < n {
        let b = d[i];
        if b == b'\\' {
            i += 1;
            if i >= n {
                break;
            }
            let e = d[i];
            match e {
                b'n' => {
                    out.push(b'\n');
                    i += 1;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 1;
                }
                b't' => {
                    out.push(b'\t');
                    i += 1;
                }
                b'b' => {
                    out.push(8);
                    i += 1;
                }
                b'f' => {
                    out.push(12);
                    i += 1;
                }
                b'(' | b')' | b'\\' => {
                    out.push(e);
                    i += 1;
                }
                b'\r' => {
                    i += 1;
                    if i < n && d[i] == b'\n' {
                        i += 1;
                    }
                }
                b'\n' => {
                    i += 1;
                }
                b'0'..=b'7' => {
                    let mut v: u32 = 0;
                    let mut k = 0;
                    while k < 3 && i < n && (b'0'..=b'7').contains(&d[i]) {
                        v = v * 8 + (d[i] - b'0') as u32;
                        i += 1;
                        k += 1;
                    }
                    out.push((v & 0xFF) as u8);
                }
                other => {
                    out.push(other);
                    i += 1;
                }
            }
            continue;
        }
        if b == b'(' {
            depth += 1;
            out.push(b'(');
            i += 1;
            continue;
        }
        if b == b')' {
            depth -= 1;
            i += 1;
            if depth <= 0 {
                break;
            }
            out.push(b')');
            continue;
        }
        out.push(b);
        i += 1;
    }
    (out, i)
}

/// 解析十六进制字符串（`<48656C6C6F>`）。
fn read_hex_string(d: &[u8], start: usize) -> (Vec<u8>, usize) {
    let n = d.len();
    let mut out: Vec<u8> = Vec::new();
    let mut i = start + 1;
    let mut hi: Option<u8> = None;
    while i < n {
        let b = d[i];
        if b == b'>' {
            i += 1;
            break;
        }
        if let Some(v) = hex_val(b) {
            match hi {
                None => hi = Some(v),
                Some(h) => {
                    out.push(h * 16 + v);
                    hi = None;
                }
            }
        }
        i += 1;
    }
    if let Some(h) = hi {
        out.push(h * 16);
    }
    (out, i)
}

fn skip_ws_fwd(d: &[u8], i: &mut usize) {
    let n = d.len();
    while *i < n && is_ws(d[*i]) {
        *i += 1;
    }
}

/// 读一个纯无符号整数（用于 `N G R` 的 G 部分）。
fn read_plain_uint(d: &[u8], i: &mut usize) -> Option<u16> {
    let n = d.len();
    let start = *i;
    let mut v: u64 = 0;
    let mut k = 0;
    while *i < n && d[*i].is_ascii_digit() {
        v = v * 10 + (d[*i] - b'0') as u64;
        *i += 1;
        k += 1;
        if k > 5 || v > u16::MAX as u64 {
            *i = start;
            return None;
        }
    }
    if *i == start {
        return None;
    }
    Some(v as u16)
}

fn read_number_val(d: &[u8], start: usize, allow_refs: bool) -> (Val, usize) {
    let n = d.len();
    let mut i = start;
    while i < n
        && (d[i].is_ascii_digit() || d[i] == b'+' || d[i] == b'-' || d[i] == b'.')
    {
        i += 1;
    }
    let txt = String::from_utf8_lossy(&d[start..i]);
    let real: f64 = match txt.parse::<f64>() {
        Ok(v) => v,
        Err(_) => 0.0,
    };
    let is_int = !txt.contains('.');

    if is_int && allow_refs && real >= 0.0 && real <= u32::MAX as f64 {
        let iv = real as i64;
        if real == iv as f64 {
            let mut j = i;
            skip_ws_fwd(d, &mut j);
            if let Some(gen) = read_plain_uint(d, &mut j) {
                skip_ws_fwd(d, &mut j);
                if j < n
                    && d[j] == b'R'
                    && (j + 1 >= n || is_token_end(d[j + 1]))
                {
                    return (Val::Ref(iv as u32, gen), j + 1);
                }
            }
        }
    }

    if is_int {
        (Val::Int(real as i64), i)
    } else {
        (Val::Real(real), i)
    }
}

// ───────────────────────── 对象扫描 ─────────────────────────

struct RawObject {
    dict: Val,
    /// 仍然处于“编码后”状态的流字节。
    raw_stream: Option<Vec<u8>>,
}

/// 在对象体里找出「深度 0 且不在字符串中」的 `stream` 与 `endobj` 位置。
///
/// `lim`：撞到扫描上限时记一笔日志。
fn scan_obj_body(data: &[u8], start: usize, lim: &mut LimitLog) -> (Option<usize>, Option<usize>) {
    let n = data.len();
    // 单对象最多扫这么远。畸形 PDF 会放几千个既不写 `endobj` 也不写 `stream` 的对象，
    // 旧代码每次都从对象头扫到文件尾 → O(n²)，200 MB 的文件稳稳耗满 60 秒超时。
    let limit = start.saturating_add(MAX_OBJ_BODY_SCAN).min(n);
    let mut i = start;
    let mut ddepth: i32 = 0;
    let mut pdepth: i32 = 0;
    let mut in_hex = false;
    let mut stream_pos: Option<usize> = None;
    let mut endobj_pos: Option<usize> = None;

    while i < limit {
        let b = data[i];

        if pdepth > 0 {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'(' {
                pdepth += 1;
            } else if b == b')' {
                pdepth -= 1;
            }
            i += 1;
            continue;
        }
        if in_hex {
            if b == b'>' {
                in_hex = false;
            }
            i += 1;
            continue;
        }

        if b == b'(' {
            pdepth += 1;
            i += 1;
            continue;
        }
        if b == b'<' {
            if i + 1 < n && data[i + 1] == b'<' {
                ddepth += 1;
                i += 2;
                continue;
            }
            in_hex = true;
            i += 1;
            continue;
        }
        if b == b'>' {
            if i + 1 < n && data[i + 1] == b'>' {
                if ddepth > 0 {
                    ddepth -= 1;
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if ddepth == 0 {
            let before_ok = i == 0 || is_token_end(data[i - 1]);
            if before_ok && b == b's' && i + 6 <= n && &data[i..i + 6] == b"stream" {
                let after = i + 6;
                if after >= n || is_token_end(data[after]) {
                    if stream_pos.is_none() {
                        stream_pos = Some(i);
                        // 字典到此结束。`endobj` 只可能出现在 `stream` 之后
                        // （出现在之前的话上面已经 break 了），对 use_stream 的判断没有任何影响，
                        // 所以立刻收工 —— 不要穿过流的二进制数据继续往后找 `endobj`：
                        // 一张几 MB 的位图会被逐字节扫一遍，纯白烧 CPU，几千个流就是几十秒。
                        return (stream_pos, None);
                    }
                }
            }
            if before_ok && b == b'e' && i + 6 <= n && &data[i..i + 6] == b"endobj" {
                let after = i + 6;
                if after >= n || is_token_end(data[after]) {
                    endobj_pos = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    if i >= limit && limit < n {
        // 扫到上限还没收工：这个对象不完整（或是字典大得离谱），按畸形对象处理。
        lim.hit(
            "obj_body_scan",
            &format!(
                "对象（偏移 {}）在 {} KB 内既没找到 stream 也没找到 endobj，已按畸形对象跳过",
                start,
                MAX_OBJ_BODY_SCAN / 1024
            ),
        );
    }
    (stream_pos, endobj_pos)
}

fn endstream_at(data: &[u8], at: usize) -> bool {
    let mut i = at;
    let mut skipped = 0;
    while i < data.len() && skipped < 4 && is_ws(data[i]) {
        i += 1;
        skipped += 1;
    }
    data.len() >= i + 9 && &data[i..i + 9] == b"endstream"
}

/// 取 `stream` / `endstream` 之间的原始字节；返回（原始字节, endstream 之后的位置）。
fn extract_stream(
    data: &[u8],
    stream_kw: usize,
    dict: &Val,
    lim: &mut LimitLog,
    search_left: &mut usize,
) -> Option<(Vec<u8>, usize)> {
    let n = data.len();
    let mut p = stream_kw + 6;
    // stream 关键字之后紧跟的单个 EOL 不属于数据。
    if p < n && data[p] == b'\r' {
        p += 1;
    }
    if p < n && data[p] == b'\n' {
        p += 1;
    }

    // 优先信任 /Length（直连整数时）。
    if let Some(len) = v_get(dict, b"Length").and_then(v_int) {
        if len >= 0 {
            let cand = p.saturating_add(len as usize);
            if cand <= n && endstream_at(data, cand) {
                let next = match find_keyword(data, cand, b"endstream") {
                    Some(e) => e + 9,
                    None => cand,
                };
                return Some((data[p..cand].to_vec(), next));
            }
        }
    }

    // 退化：向后找 endstream，并把紧贴它的 EOL 从数据里去掉。
    // 搜索距离有两道闸：单次不超过 MAX_STREAM_SCAN（防一个大文件里的一次超长搜索），
    // 累计不超过 MAX_STREAM_SEARCH_TOTAL（防几千个对象各自扫到文件尾的 O(n²)）。
    if *search_left == 0 {
        lim.hit(
            "stream_search_budget",
            &format!(
                "「找不到 endstream」的累计搜索预算（{} MB）已用完，剩下的无 /Length 流一律丢弃",
                MAX_STREAM_SEARCH_TOTAL / (1024 * 1024)
            ),
        );
        return None;
    }
    let allowed = MAX_STREAM_SCAN.min(*search_left).min(n - p.min(n));
    if allowed == 0 {
        return None;
    }
    let search_end = p + allowed;
    let e = match find_keyword(&data[..search_end], p, b"endstream") {
        Some(e) => e,
        None => {
            // 这次白扫的字节数记到预算上。成功的搜索不记账：
            // 它会把扫描位置推过 endstream，整体天然是 O(文件大小)。
            *search_left = search_left.saturating_sub(search_end - p);
            lim.hit(
                "stream_scan",
                &format!(
                    "流的 {} 字节搜索范围内没有 endstream，已丢弃该流（缺 /Length 的畸形文件）",
                    search_end - p
                ),
            );
            return None;
        }
    };
    let mut end = e;
    if end >= p + 2 && &data[end - 2..end] == b"\r\n" {
        end -= 2;
    } else if end >= p + 1 && (data[end - 1] == b'\n' || data[end - 1] == b'\r') {
        end -= 1;
    }
    if end < p {
        end = p;
    }
    Some((data[p..end].to_vec(), e + 9))
}

fn scan_objects(data: &[u8], deadline: Instant, lim: &mut LimitLog) -> Vec<(u32, RawObject)> {
    let mut out: Vec<(u32, RawObject)> = Vec::new();
    let n = data.len();
    let mut i = 0usize;
    // 「缺 /Length，只能向后搜 endstream」的累计预算（见 MAX_STREAM_SEARCH_TOTAL）。
    let mut stream_search_left: usize = MAX_STREAM_SEARCH_TOTAL;

    while i + 3 <= n && out.len() < MAX_OBJECTS {
        if Instant::now() > deadline {
            lim.hit(
                "scan_deadline",
                "扫描间接对象超过总时间上限，剩余对象已放弃（文件被判为畸形）",
            );
            break;
        }
        let at = match find_keyword(data, i, b"obj") {
            Some(a) => a,
            None => break,
        };
        let body_start = at + 3;

        // 向前回看 `num gen`
        let mut j = at;
        while j > 0 && is_ws(data[j - 1]) {
            j -= 1;
        }
        let gen_end = j;
        while j > 0 && data[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j == gen_end || gen_end - j > 10 {
            i = body_start;
            continue;
        }
        // 代号（generation）本身用不上，但必须能被解析出来，否则 `obj` 是误命中。
        let gen_ok = std::str::from_utf8(&data[j..gen_end])
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .is_some();
        if !gen_ok {
            i = body_start;
            continue;
        }
        while j > 0 && is_ws(data[j - 1]) {
            j -= 1;
        }
        let num_end = j;
        while j > 0 && data[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j == num_end || num_end - j > 10 {
            i = body_start;
            continue;
        }
        let num: u32 = match std::str::from_utf8(&data[j..num_end]).ok().and_then(|s| s.parse::<u32>().ok()) {
            Some(v) => v,
            None => {
                i = body_start;
                continue;
            }
        };

        let (stream_kw, endobj_kw) = scan_obj_body(data, body_start, lim);
        let use_stream = match stream_kw {
            Some(s) => match endobj_kw {
                Some(e) => s < e,
                None => true,
            },
            None => false,
        };

        let dict_end = if use_stream {
            match stream_kw {
                Some(s) => s,
                None => body_start,
            }
        } else {
            match endobj_kw {
                Some(e) => e,
                None => {
                    let cap = body_start.saturating_add(MAX_DICT_SCAN);
                    if cap < n {
                        cap
                    } else {
                        n
                    }
                }
            }
        };

        let dict_bytes = if dict_end > body_start {
            &data[body_start..dict_end]
        } else {
            &data[body_start..body_start]
        };
        let mut dict = Val::Dict(Vec::new());
        for v in lex(dict_bytes, true, false) {
            if matches!(v, Val::Dict(_)) {
                dict = v;
                break;
            }
        }

        let mut raw_stream: Option<Vec<u8>> = None;
        let mut next_i = if use_stream { dict_end } else { match endobj_kw { Some(e) => e + 6, None => dict_end } };
        if use_stream {
            if let Some(s) = stream_kw {
                if let Some((bytes, next)) = extract_stream(data, s, &dict, lim, &mut stream_search_left)
                {
                    raw_stream = Some(bytes);
                    next_i = next;
                }
            }
        }

        out.push((num, RawObject { dict, raw_stream }));
        i = if next_i > body_start { next_i } else { body_start };
    }
    if out.len() >= MAX_OBJECTS {
        // 对象数上限是既有保护。达到它说明文件远超正常规模，留痕便于事后判断「是不是被截断了」。
        lim.hit(
            "max_objects",
            &format!("间接对象数达到上限 {}，后面的对象已忽略", MAX_OBJECTS),
        );
    }
    out
}

// ───────────────────────── 解码流 ─────────────────────────

fn ascii_hex_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut hi: Option<u8> = None;
    for &b in data {
        if b == b'>' {
            break;
        }
        if let Some(v) = hex_val(b) {
            match hi {
                None => hi = Some(v),
                Some(h) => {
                    out.push(h * 16 + v);
                    hi = None;
                }
            }
        }
    }
    if let Some(h) = hi {
        out.push(h * 16);
    }
    Some(out)
}

fn ascii85_decode(data: &[u8], lim: &mut LimitLog) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut group: Vec<u8> = Vec::with_capacity(5);
    let mut i = 0usize;
    let n = data.len();
    // 可选的前导 <~
    while i < n && is_ws(data[i]) {
        i += 1;
    }
    if i + 1 < n && data[i] == b'<' && data[i + 1] == b'~' {
        i += 2;
    }
    while i < n {
        let b = data[i];
        i += 1;
        if is_ws(b) {
            continue;
        }
        if b == b'~' {
            break;
        }
        if b == b'z' && group.is_empty() {
            // `z` 一个字符就膨胀成 4 个 0 字节，而且它不受 5 字节分组的影响：
            // 不在这里设限的话，几百 MB 的 `z` 能撑出一个 GB 级的 Vec（旧代码只挡住了分组路径）。
            if out.len() + 4 > MAX_STREAM_OUT {
                lim.hit(
                    "ascii85_bomb",
                    &format!(
                        "ASCII85 的 z 展开超过单流 {} MB 上限，已丢弃该流",
                        MAX_STREAM_OUT / (1024 * 1024)
                    ),
                );
                return None;
            }
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return None;
        }
        group.push(b - b'!');
        if group.len() == 5 {
            let mut v: u32 = 0;
            for &g in &group {
                v = v.wrapping_mul(85).wrapping_add(g as u32);
            }
            out.push((v >> 24) as u8);
            out.push((v >> 16) as u8);
            out.push((v >> 8) as u8);
            out.push(v as u8);
            if out.len() > MAX_STREAM_OUT {
                lim.hit(
                    "ascii85_bomb",
                    &format!(
                        "ASCII85 解出的字节超过单流 {} MB 上限，已丢弃该流",
                        MAX_STREAM_OUT / (1024 * 1024)
                    ),
                );
                return None;
            }
            group.clear();
        }
    }
    if !group.is_empty() {
        let k = group.len();
        for _ in k..5 {
            group.push(84);
        }
        let mut v: u32 = 0;
        for &g in &group {
            v = v.wrapping_mul(85).wrapping_add(g as u32);
        }
        let bytes = [(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8];
        let keep = k.saturating_sub(1);
        out.extend_from_slice(&bytes[..keep.min(4)]);
    }
    Some(out)
}

fn run_length_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    let n = data.len();
    while i < n {
        let l = data[i];
        i += 1;
        if l == 128 {
            break;
        }
        if l < 128 {
            let count = l as usize + 1;
            if i + count > n {
                out.extend_from_slice(&data[i..n]);
                break;
            }
            out.extend_from_slice(&data[i..i + count]);
            i += count;
        } else {
            if i >= n {
                break;
            }
            let count = 257 - l as usize;
            let b = data[i];
            i += 1;
            for _ in 0..count {
                out.push(b);
            }
        }
        if out.len() > MAX_STREAM_OUT {
            return None;
        }
    }
    Some(out)
}

/// PNG / TIFF 预测器还原。
fn apply_predictor(data: &[u8], parms: &Val) -> Option<Vec<u8>> {
    let predictor = v_get(parms, b"Predictor").and_then(v_int);
    let predictor = match predictor {
        Some(p) => p,
        None => return Some(data.to_vec()),
    };
    if predictor <= 1 {
        return Some(data.to_vec());
    }
    let colors = match v_get(parms, b"Colors").and_then(v_int) {
        Some(c) if c > 0 && c <= 32 => c as usize,
        _ => 1,
    };
    let bpc = match v_get(parms, b"BitsPerComponent").and_then(v_int) {
        Some(b) if b == 1 || b == 2 || b == 4 || b == 8 || b == 16 => b as usize,
        _ => 8,
    };
    let columns = match v_get(parms, b"Columns").and_then(v_int) {
        Some(c) if c > 0 => c as usize,
        _ => 1,
    };
    let bpp = ((colors * bpc) + 7) / 8;
    let row_len = (columns * colors * bpc + 7) / 8;
    if row_len == 0 {
        return None;
    }

    if predictor == 2 {
        // TIFF：按分量累加。
        //
        // 取模校验只属于 TIFF 分支 —— TIFF 每行正好是 row_len 字节，
        // 而 PNG（10~15）每行前面还多一个 filter 字节，`data.len() % row_len` 几乎必然 != 0。
        // 旧代码把这条校验放在分支外面，导致所有带 PNG 预测器的流被整条丢掉；
        // PDF 1.5+ 的对象流（ObjStm）常用 PNG 预测器，丢了就读不出里面的 Catalog / 页面树。
        if data.len() % row_len != 0 {
            return None;
        }
        let rows = data.len() / row_len;
        if bpc != 8 {
            return Some(data.to_vec());
        }
        let mut out = data.to_vec();
        for r in 0..rows {
            let base = r * row_len;
            for c in bpp..row_len {
                out[base + c] = out[base + c].wrapping_add(out[base + c - bpp]);
            }
        }
        return Some(out);
    }

    if !(10..=15).contains(&predictor) {
        return Some(data.to_vec());
    }
    // PNG：每行行首多一个 filter 字节
    let stride = row_len + 1;
    if data.len() % stride != 0 {
        return None;
    }
    let rows = data.len() / stride;
    let mut out: Vec<u8> = Vec::with_capacity(rows * row_len);
    let mut prev = vec![0u8; row_len];
    for r in 0..rows {
        let base = r * stride;
        let ft = data[base];
        let mut cur = vec![0u8; row_len];
        cur.copy_from_slice(&data[base + 1..base + stride]);
        match ft {
            0 => {}
            1 => {
                for i in bpp..row_len {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            2 => {
                for i in 0..row_len {
                    cur[i] = cur[i].wrapping_add(prev[i]);
                }
            }
            3 => {
                for i in 0..row_len {
                    let left = if i >= bpp { cur[i - bpp] as u32 } else { 0 };
                    let up = prev[i] as u32;
                    cur[i] = cur[i].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                for i in 0..row_len {
                    let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
                    let b = prev[i] as i32;
                    let c = if i >= bpp { prev[i - bpp] as i32 } else { 0 };
                    let p = a + b - c;
                    let pa = (p - a).abs();
                    let pb = (p - b).abs();
                    let pc = (p - c).abs();
                    let pr = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    cur[i] = cur[i].wrapping_add(pr as u8);
                }
            }
            _ => return None,
        }
        out.extend_from_slice(&cur);
        prev = cur;
    }
    Some(out)
}

fn collect_filters(dict: &Val) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match v_get(dict, b"Filter") {
        Some(Val::Name(s)) => out.push(s.clone()),
        Some(Val::Array(items)) => {
            for it in items {
                if let Some(n) = v_name(it) {
                    out.push(n.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

fn collect_parms(dict: &Val) -> Vec<Val> {
    match v_get(dict, b"DecodeParms") {
        Some(Val::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

fn inflate_with_zlib(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = ZlibDecoder::new(data);
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let start = Instant::now();
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                out.extend_from_slice(&buf[..k]);
                if out.len() > MAX_STREAM_OUT {
                    return None;
                }
                if start.elapsed() > STREAM_TIMEOUT {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn inflate_with_raw(data: &[u8]) -> Option<Vec<u8>> {
    let mut dec = DeflateDecoder::new(data);
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let start = Instant::now();
    loop {
        match dec.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                out.extend_from_slice(&buf[..k]);
                if out.len() > MAX_STREAM_OUT {
                    return None;
                }
                if start.elapsed() > STREAM_TIMEOUT {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// FlateDecode 是 **zlib** 包装（不是 gzip）；个别生产商写裸 deflate，作为兜底再试一次。
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    match inflate_with_zlib(data) {
        Some(v) => Some(v),
        None => inflate_with_raw(data),
    }
}

/// 这个流值不值得解压？图像 / xref / 元数据流直接跳过，
/// 否则扫描件 PDF（几十 MB 位图）会把时间浪费在白解压上。
fn should_decode_stream(dict: &Val) -> bool {
    let sub = v_name_or(v_get(dict, b"Subtype"), "");
    if sub == "Image" {
        return false;
    }
    let t = v_name_or(v_get(dict, b"Type"), "");
    if t == "XRef" || t == "Metadata" || t == "EmbeddedFile" {
        return false;
    }
    for f in collect_filters(dict) {
        if matches!(
            f.as_str(),
            "DCTDecode" | "JPXDecode" | "CCITTFaxDecode" | "JBIG2Decode"
        ) {
            return false;
        }
    }
    true
}

fn decode_stream(dict: &Val, raw: &[u8], lim: &mut LimitLog) -> Option<Vec<u8>> {
    let filters = collect_filters(dict);
    if filters.is_empty() {
        return Some(raw.to_vec());
    }
    let parms = collect_parms(dict);
    let mut data = raw.to_vec();
    for (idx, f) in filters.iter().enumerate() {
        let is_flate = f == "FlateDecode" || f == "Fl";
        data = match f.as_str() {
            "FlateDecode" | "Fl" => inflate(&data)?,
            "ASCIIHexDecode" | "AHx" => ascii_hex_decode(&data)?,
            "ASCII85Decode" | "A85" => ascii85_decode(&data, lim)?,
            "RunLengthDecode" | "RL" => run_length_decode(&data)?,
            // 图像 / 不支持：跳过该流
            _ => return None,
        };
        if is_flate {
            if let Some(p) = parms.get(idx) {
                data = apply_predictor(&data, p)?;
            }
        }
    }
    Some(data)
}

// ───────────────────────── 文档 ─────────────────────────

struct Doc {
    order: Vec<u32>,
    vals: HashMap<u32, Val>,
    streams: HashMap<u32, Option<Vec<u8>>>,
}

impl Doc {
    fn build(data: &[u8], deadline: Instant) -> Doc {
        Doc::build_with_budget(data, deadline, MAX_TOTAL_DECOMPRESSED_BYTES)
    }

    /// 真正的构造函数；`stream_budget` 是「所有流解压后的总字节」预算。
    ///
    /// 单独抽出来是为了让测试能灌一个很小的预算来验证炸弹确实被拦住 ——
    /// 正常调用一律传 `MAX_TOTAL_DECOMPRESSED_BYTES`，走的是同一条代码路径。
    fn build_with_budget(data: &[u8], deadline: Instant, stream_budget: usize) -> Doc {
        let mut lim = LimitLog::default();
        let raws = scan_objects(data, deadline, &mut lim);
        let mut doc = Doc {
            order: Vec::with_capacity(raws.len()),
            vals: HashMap::with_capacity(raws.len()),
            streams: HashMap::with_capacity(raws.len()),
        };
        // 已经解压出来的总字节。单个流各有 MAX_STREAM_OUT 上限，但「几百个都合法的小炸弹」
        // 加起来照样能把进程 OOM（内存耗尽时进程被系统直接终止，捕获不到），所以要有全局预算。
        let mut decoded_total: usize = 0;
        for (num, raw) in raws {
            if Instant::now() > deadline {
                lim.hit(
                    "build_deadline",
                    "建立对象索引超过总时间上限，剩余对象已放弃",
                );
                break;
            }
            if !doc.vals.contains_key(&num) {
                doc.order.push(num);
            }
            let mut decoded = match &raw.raw_stream {
                Some(bytes) => {
                    if !should_decode_stream(&raw.dict) {
                        None
                    } else if decoded_total >= stream_budget {
                        // 预算已用完：连解压都不做，直接当这个流不存在（省掉解压的时间与峰值内存）。
                        lim.hit(
                            "decompress_budget",
                            &format!(
                                "解压总字节达到 {} MB 预算，剩余流不再解压",
                                stream_budget / (1024 * 1024)
                            ),
                        );
                        None
                    } else {
                        decode_stream(&raw.dict, bytes, &mut lim)
                    }
                }
                None => None,
            };
            if let Some(b) = &decoded {
                if decoded_total.saturating_add(b.len()) > stream_budget {
                    // 这个流本身合法，但加上它就超预算：结果不驻留（解压出来的临时内存随即释放）。
                    lim.hit(
                        "decompress_budget",
                        &format!(
                            "解压总字节达到 {} MB 预算，剩余流不再保留",
                            stream_budget / (1024 * 1024)
                        ),
                    );
                    decoded = None;
                } else {
                    decoded_total += b.len();
                }
            }
            doc.vals.insert(num, raw.dict);
            doc.streams.insert(num, decoded);
        }

        // PDF 1.5+ 常把 Catalog / Pages / 字体字典放进对象流（/Type /ObjStm），
        // 不展开的话就找不到页面树和 ToUnicode，中文会变成乱码。
        let objstm_nums: Vec<u32> = doc
            .order
            .iter()
            .cloned()
            .filter(|n| doc.is_type(*n, "ObjStm"))
            .collect();
        for n in objstm_nums {
            if Instant::now() > deadline {
                lim.hit(
                    "build_deadline",
                    "展开对象流超过总时间上限，剩余对象流已放弃",
                );
                break;
            }
            doc.expand_objstm(n, deadline, &mut lim);
        }
        doc
    }

    /// 展开一个 `/Type /ObjStm` 对象流，把里面装的对象补进 `vals`。
    fn expand_objstm(&mut self, num: u32, deadline: Instant, lim: &mut LimitLog) {
        let data = match self.stream(num) {
            Some(b) => b.to_vec(),
            None => return,
        };
        let dict = match self.vals.get(&num) {
            Some(d) => d.clone(),
            None => return,
        };
        let count = match v_get(&dict, b"N").and_then(v_int) {
            Some(v) if v > 0 => v as usize,
            _ => return,
        };
        let first = match v_get(&dict, b"First").and_then(v_int) {
            Some(v) if v >= 0 => v as usize,
            _ => return,
        };
        if first > data.len() || count > MAX_OBJECTS {
            lim.hit(
                "objstm_header",
                &format!(
                    "对象流 {} 的 /First={} /N={} 不可信（上限 {}），整个对象流已跳过",
                    num, first, count, MAX_OBJECTS
                ),
            );
            return;
        }

        // 头部是 count 组 `对象号 偏移`（偏移相对 First）
        let header = lex(&data[..first], false, false);
        let mut nums: Vec<i64> = Vec::new();
        for v in &header {
            if let Val::Int(i) = v {
                nums.push(*i);
            }
        }
        let mut pairs: Vec<(u32, usize)> = Vec::new();
        let mut k = 0usize;
        while k + 1 < nums.len() && pairs.len() < count {
            let onum = nums[k];
            let off = nums[k + 1];
            k += 2;
            if onum < 0 || off < 0 {
                continue;
            }
            if onum > u32::MAX as i64 || off as u64 > data.len() as u64 {
                continue;
            }
            pairs.push((onum as u32, off as usize));
        }

        for (i, (onum, off)) in pairs.iter().enumerate() {
            if Instant::now() > deadline {
                lim.hit(
                    "objstm_deadline",
                    "展开对象流超过总时间上限，该对象流剩余对象已放弃",
                );
                return;
            }
            if self.vals.contains_key(onum) {
                continue;
            }
            let start = match first.checked_add(*off) {
                Some(s) if s <= data.len() => s,
                _ => continue,
            };
            let end = match pairs.get(i + 1) {
                Some((_, next_off)) => match first.checked_add(*next_off) {
                    Some(e) if e <= data.len() && e > start => e,
                    _ => data.len(),
                },
                None => data.len(),
            };
            if end <= start {
                continue;
            }
            let mut val = Val::Null;
            for v in lex(&data[start..end], true, false) {
                match &v {
                    Val::Op(_) => continue,
                    _ => {
                        val = v;
                        break;
                    }
                }
            }
            self.order.push(*onum);
            self.vals.insert(*onum, val);
            self.streams.insert(*onum, None);
        }
    }

    fn stream(&self, num: u32) -> Option<&[u8]> {
        match self.streams.get(&num) {
            Some(Some(b)) => Some(b.as_slice()),
            _ => None,
        }
    }

    fn is_type(&self, num: u32, want: &str) -> bool {
        match self.vals.get(&num) {
            Some(d) => v_name_or(v_get(d, b"Type"), "") == want,
            None => false,
        }
    }

    fn find_type(&self, want: &str) -> Option<u32> {
        for n in &self.order {
            if self.is_type(*n, want) {
                return Some(*n);
            }
        }
        None
    }

    fn walk_pages(
        &self,
        num: u32,
        out: &mut Vec<u32>,
        visited: &mut HashSet<u32>,
        visits: &mut usize,
        depth: usize,
        lim: &mut LimitLog,
    ) {
        if depth > MAX_TREE_DEPTH {
            lim.hit(
                "tree_depth",
                &format!(
                    "页面树深度超过 {} 层，停止下钻（正常文件的页面树只有十来层）",
                    MAX_TREE_DEPTH
                ),
            );
            return;
        }
        if out.len() >= MAX_PAGES {
            return;
        }
        // 同一个节点只展开一次。`/Kids [2 0 R 2 0 R 2 0 R 2 0 R]` 自己引用自己时，
        // 只有深度上限的话深度 32 下仍要做约 4^32 次递归 —— 线程会被永久占满 CPU。
        if !visited.insert(num) {
            lim.hit(
                "tree_revisit",
                &format!("页面树里的对象 {} 被重复引用，已跳过（疑似自引用或成环）", num),
            );
            return;
        }
        *visits += 1;
        if *visits > MAX_TREE_VISITS {
            lim.hit(
                "tree_visits",
                &format!(
                    "页面树访问次数超过上限 {}，停止遍历",
                    MAX_TREE_VISITS
                ),
            );
            return;
        }
        let d = match self.vals.get(&num) {
            Some(d) => d,
            None => return,
        };
        if self.is_type(num, "Page") {
            out.push(num);
            return;
        }
        if let Some(Val::Array(kids)) = v_get(d, b"Kids") {
            for k in kids {
                if out.len() >= MAX_PAGES {
                    return;
                }
                if let Some(r) = v_ref(k) {
                    self.walk_pages(r, out, visited, visits, depth + 1, lim);
                }
            }
        }
    }

    fn page_order(&self, lim: &mut LimitLog) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        let mut visited: HashSet<u32> = HashSet::new();
        let mut visits: usize = 0;
        if let Some(cat) = self.find_type("Catalog") {
            if let Some(d) = self.vals.get(&cat) {
                if let Some(r) = v_get(d, b"Pages").and_then(v_ref) {
                    self.walk_pages(r, &mut out, &mut visited, &mut visits, 0, lim);
                }
            }
        }
        if out.is_empty() {
            // 走到这里可能是「页面树成环被拦住」，也可能是文件根本没有页面树；
            // 上面已经记过日志了，这里按 /Type /Page 兜底。
            for num in &self.order {
                if self.is_type(*num, "Page") {
                    out.push(*num);
                    if out.len() >= MAX_PAGES {
                        break;
                    }
                }
            }
        }
        if out.len() >= MAX_PAGES {
            // 页数上限是既有保护（扫描件 PDF 可能声称有几千页）。它也是「上限」，同样要留痕。
            lim.hit(
                "pages_cap",
                &format!("页面数达到上限 {}，超出的页面已忽略", MAX_PAGES),
            );
        }
        out
    }

    fn inherited_resources(&self, dict: &Val, depth: usize) -> Option<Val> {
        if depth > MAX_TREE_DEPTH {
            // /Parent 成环时靠深度收口（只走父链，不是树，不会爆炸，但也不该静默返回）。
            note_limit(&format!(
                "/Resources 继承链超过 {} 层，按没有资源处理",
                MAX_TREE_DEPTH
            ));
            return None;
        }
        if let Some(r) = v_get(dict, b"Resources") {
            return Some(r.clone());
        }
        if let Some(p) = v_get(dict, b"Parent").and_then(v_ref) {
            if let Some(pd) = self.vals.get(&p) {
                return self.inherited_resources(pd, depth + 1);
            }
        }
        None
    }

    fn load_cmap(&self, font_num: u32) -> Option<Rc<CmapTable>> {
        let d = self.vals.get(&font_num)?;
        let tu = v_get(d, b"ToUnicode")?;
        let r = v_ref(tu)?;
        let bytes = self.stream(r)?;
        let table = parse_cmap(bytes)?;
        Some(Rc::new(table))
    }

    fn font_info(&self, num: u32) -> FontInfo {
        let cmap = self.load_cmap(num);
        if cmap.is_some() {
            let two = match &cmap {
                Some(c) => c.code_bytes >= 2,
                None => false,
            };
            return FontInfo { cmap, two_byte: two };
        }
        let d = match self.vals.get(&num) {
            Some(d) => d,
            None => {
                return FontInfo {
                    cmap: None,
                    two_byte: false,
                }
            }
        };
        let st = v_name_or(v_get(d, b"Subtype"), "");
        let enc = v_name_or(v_get(d, b"Encoding"), "");
        let two = st == "Type0"
            || enc == "Identity-H"
            || enc == "Identity-V"
            || v_get(d, b"DescendantFonts").is_some();
        FontInfo { cmap: None, two_byte: two }
    }

    fn build_font_map(
        &self,
        res: &Val,
        cache: &mut HashMap<u32, Rc<FontInfo>>,
    ) -> HashMap<String, Rc<FontInfo>> {
        let mut out: HashMap<String, Rc<FontInfo>> = HashMap::new();
        let fonts = match v_get(res, b"Font") {
            Some(f) => f,
            None => return out,
        };
        let entries = match fonts {
            Val::Dict(e) => e,
            _ => return out,
        };
        for (name, v) in entries {
            let n = match v_ref(v) {
                Some(n) => n,
                None => continue,
            };
            let info = match cache.get(&n) {
                Some(i) => i.clone(),
                None => {
                    let i = Rc::new(self.font_info(n));
                    cache.insert(n, i.clone());
                    i
                }
            };
            out.insert(name.clone(), info);
        }
        out
    }

    fn form_parts(&self, num: u32, fallback_res: &Val) -> Option<(Vec<u8>, Val)> {
        let d = self.vals.get(&num)?;
        if v_name_or(v_get(d, b"Subtype"), "") != "Form" {
            return None;
        }
        let bytes = self.stream(num)?.to_vec();
        let res = match v_get(d, b"Resources") {
            Some(r) => r.clone(),
            None => fallback_res.clone(),
        };
        Some((bytes, res))
    }

    fn stream_is_text_candidate(&self, num: u32) -> bool {
        let d = match self.vals.get(&num) {
            Some(d) => d,
            None => return false,
        };
        let t = v_name_or(v_get(d, b"Type"), "");
        let s = v_name_or(v_get(d, b"Subtype"), "");
        let skip_t = matches!(
            t,
            "XRef" | "ObjStm" | "Metadata" | "EmbeddedFile" | "Filespec" | "Font" | "FontDescriptor"
        );
        let skip_s = matches!(
            s,
            "Image" | "XML" | "Type1C" | "CIDFontType0C" | "OpenType" | "TrueType"
        );
        !skip_t && !skip_s
    }

    fn render_page(
        &self,
        num: u32,
        cmaps: &mut HashMap<u32, Rc<FontInfo>>,
        deadline: Instant,
        lim: &mut LimitLog,
    ) -> String {
        let dict = match self.vals.get(&num) {
            Some(d) => d,
            None => return String::new(),
        };
        let res = match self.inherited_resources(dict, 0) {
            Some(r) => r,
            None => Val::Dict(Vec::new()),
        };
        let fonts = self.build_font_map(&res, cmaps);

        let content = self.collect_contents(dict, lim);
        if content.is_empty() {
            return String::new();
        }
        let mut ctx = Ctx::new(self, deadline);
        let mut guard = RenderGuard::new();
        render_content(&mut ctx, &mut guard, cmaps, &content, &fonts, &res, 0);
        ctx.out
    }

    /// 把 `/Contents`（单个流或数组）拼成一页的内容流。
    ///
    /// 数组可以重复引用同一个流：恶意文件把一个解压后 63 MB 的炸弹引用 500 次（≈32 GB），
    /// 旧代码会一路 `extend_from_slice` 把内存吃光，所以拼到 `MAX_CONTENT_BYTES` 就停手并记日志。
    /// 单个流本身的大小仍由 `MAX_STREAM_OUT` 管，这里是「拼接结果」的第二道闸。
    fn collect_contents(&self, dict: &Val, lim: &mut LimitLog) -> Vec<u8> {
        let refs: Vec<u32> = match v_get(dict, b"Contents") {
            Some(Val::Ref(n, _)) => vec![*n],
            Some(Val::Array(items)) => items.iter().filter_map(|it| v_ref(it)).collect(),
            _ => Vec::new(),
        };
        let mut content: Vec<u8> = Vec::new();
        for n in refs {
            let b = match self.stream(n) {
                Some(b) => b,
                None => continue,
            };
            if content
                .len()
                .saturating_add(b.len())
                .saturating_add(1)
                > MAX_CONTENT_BYTES
            {
                lim.hit(
                    "content_bytes",
                    &format!(
                        "页面 /Contents 拼接超过 {} MB（已拼 {} 字节，从对象 {} 起丢弃），疑似重复引用压缩炸弹",
                        MAX_CONTENT_BYTES / (1024 * 1024),
                        content.len(),
                        n
                    ),
                );
                break;
            }
            content.extend_from_slice(b);
            // 相邻两个内容流之间必须有分隔，否则尾部 token 和下一个流的头部会粘在一起；
            // 只有一个流时这个换行是空白，词法分析会忽略掉，不影响结果。
            content.push(b'\n');
        }
        content
    }
}

// ───────────────────────── ToUnicode CMap ─────────────────────────

#[derive(Debug, Default)]
struct CmapTable {
    map: HashMap<u32, String>,
    code_bytes: usize,
    /// `/begincodespacerange` 声明的码空间区间（lo, hi, 字节长度），按长度从大到小排。
    ///
    /// 1 字节和 2 字节混合编码的 PDF 必须靠它决定「下一个码读几个字节」：
    /// 旧实现只看全局最大码长（code_bytes），混合编码会整段解成乱码。
    spaces: Vec<(u32, u32, usize)>,
}

fn be_bytes_to_u64(b: &[u8]) -> Option<u64> {
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    let mut v: u64 = 0;
    for &x in b {
        v = (v << 8) | x as u64;
    }
    Some(v)
}

fn utf16be_string(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    if b.len() % 2 == 1 {
        return latin1_decode(b);
    }
    let mut units: Vec<u16> = Vec::with_capacity(b.len() / 2);
    let mut i = 0usize;
    while i + 1 < b.len() {
        units.push(u16::from_be_bytes([b[i], b[i + 1]]));
        i += 2;
    }
    String::from_utf16_lossy(&units)
}

fn u64_to_utf16be_string(v: u64, len: usize) -> Option<String> {
    if len == 0 || len > 4 {
        return None;
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(len);
    let mut k = len;
    while k > 0 {
        k -= 1;
        bytes.push(((v >> (8 * k)) & 0xFF) as u8);
    }
    Some(utf16be_string(&bytes))
}

fn parse_cmap(bytes: &[u8]) -> Option<CmapTable> {
    let toks = lex(bytes, false, false);
    let mut map: HashMap<u32, String> = HashMap::new();
    let mut code_bytes: usize = 0;
    let mut spaces: Vec<(u32, u32, usize)> = Vec::new();
    let mut lim = LimitLog::default();
    let mut i = 0usize;
    let mut entries: usize = 0;

    while i < toks.len() {
        let kw = match &toks[i] {
            Val::Op(s) => s.clone(),
            _ => {
                i += 1;
                continue;
            }
        };
        match kw.as_str() {
            "begincodespacerange" => {
                i += 1;
                while i + 1 < toks.len() {
                    match (&toks[i], &toks[i + 1]) {
                        (Val::Str(lo), Val::Str(hi)) => {
                            if lo.len() > code_bytes {
                                code_bytes = lo.len();
                            }
                            // 记下区间本身，解码时靠它决定码长（B27）。
                            // 两端长度必须一致、且不超 4 字节（本模块的码值上限是 u32）。
                            if !lo.is_empty() && lo.len() <= 4 && lo.len() == hi.len() {
                                if let (Some(l), Some(h)) =
                                    (be_bytes_to_u64(lo), be_bytes_to_u64(hi))
                                {
                                    if l <= h && h <= u32::MAX as u64 {
                                        spaces.push((l as u32, h as u32, lo.len()));
                                    }
                                }
                            }
                            i += 2;
                        }
                        _ => break,
                    }
                }
                i = skip_to_op(&toks, i, "endcodespacerange");
                continue;
            }
            "beginbfchar" => {
                i += 1;
                while i + 1 < toks.len() {
                    match (&toks[i], &toks[i + 1]) {
                        (Val::Str(src), Val::Str(dst)) => {
                            if src.len() > code_bytes {
                                code_bytes = src.len();
                            }
                            if let Some(code) = be_bytes_to_u64(src) {
                                let s = utf16be_string(dst);
                                if !s.is_empty() && code <= u32::MAX as u64 {
                                    map.insert(code as u32, s);
                                    entries += 1;
                                }
                            }
                            i += 2;
                        }
                        _ => break,
                    }
                    if entries > MAX_CMAP_ENTRIES {
                        break;
                    }
                }
                i = skip_to_op(&toks, i, "endbfchar");
                continue;
            }
            "beginbfrange" => {
                i += 1;
                while i + 2 < toks.len() {
                    let lo = match v_str(&toks[i]) {
                        Some(s) => s.to_vec(),
                        None => break,
                    };
                    let hi = match v_str(&toks[i + 1]) {
                        Some(s) => s.to_vec(),
                        None => break,
                    };
                    if lo.len() > code_bytes {
                        code_bytes = lo.len();
                    }
                    match &toks[i + 2] {
                        Val::Str(dst) => {
                            let lo_v = be_bytes_to_u64(&lo);
                            let hi_v = be_bytes_to_u64(&hi);
                            let base = be_bytes_to_u64(dst);
                            let dlen = dst.len();
                            // 目标串只有 1～4 字节才还原得成 UTF-16BE。5～8 字节时
                            // u64_to_utf16be_string 恒返回 None、entries 永远不增长，
                            // 循环会空转约 43 亿次（`beginbfrange <00000000> <FFFFFFFF> <5 字节目标>`）。
                            if dlen == 0 || dlen > 4 {
                                lim.hit(
                                    "bfrange_dlen",
                                    &format!(
                                        "beginbfrange 的目标串长 {} 字节（只支持 1-4），该区间已跳过",
                                        dlen
                                    ),
                                );
                                i += 3;
                                continue;
                            }
                            if let (Some(l), Some(h), Some(b)) = (lo_v, hi_v, base) {
                                let mut c = l;
                                while c <= h && entries <= MAX_CMAP_ENTRIES && c <= u32::MAX as u64 {
                                    let v = b.wrapping_add(c - l);
                                    if let Some(s) = u64_to_utf16be_string(v, dlen) {
                                        if !s.is_empty() {
                                            map.insert(c as u32, s);
                                            entries += 1;
                                        }
                                    }
                                    c += 1;
                                }
                            }
                            i += 3;
                        }
                        Val::Array(items) => {
                            if let Some(l) = be_bytes_to_u64(&lo) {
                                for (k, it) in items.iter().enumerate() {
                                    if let Some(d) = v_str(it) {
                                        let s = utf16be_string(d);
                                        let code = l.wrapping_add(k as u64);
                                        if !s.is_empty() && code <= u32::MAX as u64 {
                                            map.insert(code as u32, s);
                                            entries += 1;
                                        }
                                    }
                                }
                            }
                            i += 3;
                        }
                        _ => break,
                    }
                    if entries > MAX_CMAP_ENTRIES {
                        break;
                    }
                }
                i = skip_to_op(&toks, i, "endbfrange");
                continue;
            }
            _ => {
                i += 1;
            }
        }
    }

    if map.is_empty() {
        return None;
    }
    if entries > MAX_CMAP_ENTRIES {
        // 条数上限是既有保护（畸形 CMap 可以声明几十万条映射）。同样是上限，也留痕。
        lim.hit(
            "cmap_entries",
            &format!(
                "CMap 映射条数超过上限 {}，剩余的已忽略",
                MAX_CMAP_ENTRIES
            ),
        );
    }
    if code_bytes == 0 {
        code_bytes = 1;
    }
    if code_bytes > 4 {
        code_bytes = 4;
    }
    // 长的码空间排前面：解码时优先尝试最长的前缀，短的留给它匹配不上的字节。
    spaces.sort_by(|a, b| b.2.cmp(&a.2));
    spaces.dedup();
    Some(CmapTable {
        map,
        code_bytes,
        spaces,
    })
}

fn skip_to_op(toks: &[Val], from: usize, want: &str) -> usize {
    let mut i = from;
    while i < toks.len() {
        if let Val::Op(o) = &toks[i] {
            if o == want {
                return i + 1;
            }
        }
        i += 1;
    }
    i
}

impl CmapTable {
    fn decode(&self, bytes: &[u8]) -> String {
        let mut s = String::new();
        if self.spaces.is_empty() {
            // 文件没声明码空间：退回旧的「按最大码长定宽」策略，行为与加固前完全一致。
            if self.code_bytes >= 2 {
                let mut i = 0usize;
                while i + 1 < bytes.len() {
                    let code = ((bytes[i] as u32) << 8) | bytes[i + 1] as u32;
                    if let Some(v) = self.map.get(&code) {
                        s.push_str(v);
                    }
                    i += 2;
                }
            } else {
                for &b in bytes {
                    match self.map.get(&(b as u32)) {
                        Some(v) => s.push_str(v),
                        None => s.push(winansi_char(b)),
                    }
                }
            }
            return s;
        }

        // 有码空间：按前缀匹配决定码长（先试最长的）。
        // 混合编码的 PDF（比如 ASCII 用 1 字节、汉字用 2 字节）就靠这一步才不会解成乱码。
        let mut i = 0usize;
        while i < bytes.len() {
            let (code, len) = match self.match_code(&bytes[i..]) {
                Some(x) => x,
                // 剩下的字节连最短的码空间都对不上：文档畸形。
                // 直接收工（旧定宽实现遇到这种尾巴同样是丢掉的），绝不能原地打转。
                None => break,
            };
            if let Some(v) = self.map.get(&code) {
                s.push_str(v);
            } else if len == 1 {
                // 单字节码没有映射时按 WinAnsi 兜底（混合编码里的 ASCII 常这样走）；
                // 多字节码保持旧行为：不猜，跳过。
                s.push(winansi_char(bytes[i]));
            }
            i += len;
        }
        s
    }

    /// 在码空间里找匹配 `rest` 最长前缀的码；返回（码值, 用掉几个字节）。
    ///
    /// `spaces` 已按长度降序排好，所以第一个命中的就是最长前缀。
    fn match_code(&self, rest: &[u8]) -> Option<(u32, usize)> {
        for &(lo, hi, len) in &self.spaces {
            if rest.len() < len {
                continue;
            }
            let mut code: u32 = 0;
            for &b in &rest[..len] {
                code = (code << 8) | b as u32;
            }
            if code >= lo && code <= hi {
                return Some((code, len));
            }
        }
        None
    }
}

struct FontInfo {
    cmap: Option<Rc<CmapTable>>,
    two_byte: bool,
}

impl FontInfo {
    fn decode(&self, bytes: &[u8]) -> String {
        if let Some(t) = &self.cmap {
            return t.decode(bytes);
        }
        if self.two_byte {
            return utf16be_string(bytes);
        }
        fallback_decode(bytes)
    }
}

// ───────────────────────── 无 ToUnicode 时的回退解码 ─────────────────────────

/// 汉字 / 假名 / 韩文 / 全角标点等“东亚文字”判定（用于无 ToUnicode 时的兜底选择）。
fn is_east_asian(c: char) -> bool {
    let u = c as u32;
    (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0x3040..=0x30FF).contains(&u)
        || (0xAC00..=0xD7AF).contains(&u)
        || (0x3000..=0x303F).contains(&u)
        || (0xFF00..=0xFFEF).contains(&u)
        || (0x20000..=0x2FA1F).contains(&u)
}

/// 只认汉字/假名/全角标点。判断「UTF-16BE 猜测是否可信」时用它：
/// 韩文区（U+AC00..U+D7AF）特别容易被 GBK 双字节误撞，所以排除掉。
fn is_han_or_kana(c: char) -> bool {
    let u = c as u32;
    (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0x3040..=0x30FF).contains(&u)
        || (0x3000..=0x303F).contains(&u)
        || ((0xFF00..=0xFFEF).contains(&u) && !(0xFF61..=0xFF9F).contains(&u))
        || (0x20000..=0x2FA1F).contains(&u)
}

fn is_cjk(c: char) -> bool {
    is_east_asian(c)
}

fn winansi_char(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        0xA0 => ' ',
        _ => match char::from_u32(b as u32) {
            Some(c) => c,
            None => '\u{FFFD}',
        },
    }
}

fn latin1_decode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        s.push(winansi_char(b));
    }
    s
}

/// 没有 ToUnicode 时的兜底：先按 WinAnsi/Latin-1 逐字节；若含高位字节且长度是偶数，
/// 再用 UTF-16BE 试一次，只有当结果“以 CJK 为主”时才采用（否则会破坏普通 ASCII 文本）。
fn fallback_decode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let latin = latin1_decode(bytes);
    let has_high = bytes.iter().any(|b| *b >= 0x80);
    if has_high && bytes.len() % 2 == 0 {
        let u = utf16be_string(bytes);
        let total = u.chars().count();
        let han = u.chars().filter(|c| is_han_or_kana(*c)).count();
        if total > 0 && han >= 2 && han * 2 > total {
            return u;
        }
    }
    latin
}

// ───────────────────────── 内容流渲染 ─────────────────────────

struct Ctx<'a> {
    doc: &'a Doc,
    out: String,
    deadline: Instant,
    pending_newline: bool,
    pending_space: bool,
    // ── 文本定位状态：用来判断“这是新的一行”还是“同一行里挪了一下” ──
    /// 当前行矩阵的 y（文本空间）
    line_y: f64,
    /// 当前行矩阵的 x（文本空间）
    line_x: f64,
    /// 上次真的输出文字时的行 y
    last_text_y: Option<f64>,
    /// 上次真的输出文字时的位置与内容指纹（用于去掉伪粗体的重复绘制）
    last_emit_x: f64,
    last_emit_y: f64,
    last_emit_hash: u64,
    last_emit_len: usize,
    /// 当前字号（`Tf`）
    font_size: f64,
    /// 当前文本矩阵的 y 缩放（`Tm` 的 d 分量）
    tm_scale: f64,
    /// 累积的 CTM y 缩放（`cm`，用 q/Q 栈保存恢复）
    ctm_scale: f64,
    ctm_stack: Vec<f64>,
    /// 行距（`TD` / `TL`）
    leading: f64,
    /// 上一段输出文字的估算宽度（设备单位），用于判断词间空格
    last_text_width: f64,
    /// 最近的纯水平位移（设备单位）：可能是空格，也可能只是字距
    pending_x_advance: Option<f64>,
}

impl<'a> Ctx<'a> {
    fn new(doc: &'a Doc, deadline: Instant) -> Ctx<'a> {
        Ctx {
            doc,
            out: String::new(),
            deadline,
            pending_newline: false,
            pending_space: false,
            line_y: 0.0,
            line_x: 0.0,
            last_text_y: None,
            last_emit_x: f64::NAN,
            last_emit_y: f64::NAN,
            last_emit_hash: 0,
            last_emit_len: 0,
            font_size: 12.0,
            tm_scale: 1.0,
            ctm_scale: 1.0,
            ctm_stack: Vec::new(),
            leading: 0.0,
            last_text_width: 0.0,
            pending_x_advance: None,
        }
    }

    /// 当前字号在文本空间里的实际大小（用于换行阈值）。
    fn effective_font_size(&self) -> f64 {
        (self.font_size * self.tm_scale * self.ctm_scale).abs()
    }

    /// 输出文字前决定要不要换行：只有当行 y 真的变了才换行。
    ///
    /// 这样既能给正常文档断行，又不会把「同一条基线上逐字符 `TD` 0`」的
    /// PDF（WPS / 某些排版器导出）搞成一行一个字。
    fn note_text_position(&mut self) {
        let eff = self.effective_font_size();
        let eps = if eff > 0.0 { (eff * 0.25).max(0.5) } else { 0.5 };
        if let Some(ly) = self.last_text_y {
            if (self.line_y - ly).abs() > eps {
                self.pending_newline = true;
                self.pending_x_advance = None;
            }
        }
        self.last_text_y = Some(self.line_y);
    }

    /// 用「上一段文字估算宽度 vs 实际水平位移」判断这里是不是一个空格。
    ///
    /// 印刷驱动常把空格编码成 `Td` 的横向位移，而不是字符串里的 0x20。
    /// 逐字符定位时位移 ≈ 字宽，差值接近 0，所以不会误加空格。
    fn note_word_gap(&mut self, next: &str) {
        let adv = match self.pending_x_advance.take() {
            Some(a) => a,
            None => return,
        };
        let eff = self.effective_font_size();
        if eff <= 0.0 || self.last_text_width <= 0.0 {
            return;
        }
        let gap = adv - self.last_text_width;
        if gap <= 0.17 * eff {
            return;
        }
        let prev_cjk = match self.out.chars().last() {
            Some(c) => is_cjk(c),
            None => false,
        };
        let next_cjk = match next.chars().next() {
            Some(c) => is_cjk(c),
            None => false,
        };
        if !prev_cjk && !next_cjk {
            self.pending_space = true;
        }
    }

    fn force_newline(&mut self) {
        self.pending_newline = true;
    }

    /// 判断这段文字是不是「同一位置重复绘制」的产物（伪粗体会把每个字画两遍，
    /// 第二遍只偏移 0.05~0.15 个单位的 x）。相同内容 + 位置几乎不变 → 丢弃。
    fn is_duplicate_emit(&self, s: &str) -> bool {
        if self.last_emit_len == 0 || self.last_emit_len != s.chars().count() {
            return false;
        }
        let (h, _) = text_fingerprint(s);
        if h != self.last_emit_hash {
            return false;
        }
        let scale = (self.tm_scale * self.ctm_scale).abs();
        let eff = self.effective_font_size();
        let dx = ((self.line_x - self.last_emit_x) * scale).abs();
        let dy = ((self.line_y - self.last_emit_y) * scale).abs();
        dx <= 0.12 * eff && dy <= 0.25 * eff
    }

    fn remember_emit(&mut self, s: &str) {
        let (h, n) = text_fingerprint(s);
        self.last_emit_hash = h;
        self.last_emit_len = n;
        self.last_emit_x = self.line_x;
        self.last_emit_y = self.line_y;
    }
}

/// 文本指纹：FNV-1a + 字符数，用来识别重复绘制。
fn text_fingerprint(s: &str) -> (u64, usize) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h, s.chars().count())
}

/// 取操作数栈里倒数第 `from_end` 个操作数的数值（1 = 最后一个）。
fn stack_num(stack: &[Val], from_end: usize) -> Option<f64> {
    if from_end == 0 || stack.len() < from_end {
        return None;
    }
    match &stack[stack.len() - from_end] {
        Val::Int(i) => Some(*i as f64),
        Val::Real(r) => Some(*r),
        _ => None,
    }
}

/// Helvetica 字宽表（1/1000 em），覆盖 ASCII 32..=126。
///
/// 用途：有些 PDF（印刷驱动导出）把空格编码成 `Td` 的横向位移，
/// 需要拿「上一段文字的估算宽度」和实际位移比较，才能判断那里有没有空格。
const HELV_W: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // 32..47
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // 48..63
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // 64..79
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // 80..95
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // 96..111
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // 112..126
];

/// 估算一段文字在设备空间里的宽度（单位与字号相同）。
fn estimate_text_width(s: &str, font_size: f64) -> f64 {
    let mut em = 0.0f64;
    for c in s.chars() {
        let u = c as u32;
        if c == '\n' || c == '\r' || c == '\t' {
            continue;
        }
        if is_cjk(c) {
            em += 1.0;
        } else if (0x20..0x7F).contains(&u) {
            em += HELV_W[(u - 0x20) as usize] as f64 / 1000.0;
        } else {
            em += 0.5;
        }
    }
    em * font_size
}

fn is_ascii_word(c: Option<char>) -> bool {
    match c {
        Some(c) => c.is_ascii_alphanumeric(),
        None => false,
    }
}

fn push_text(ctx: &mut Ctx, s: &str) {
    if s.is_empty() {
        return;
    }
    if ctx.pending_newline {
        if !ctx.out.is_empty() && !ctx.out.ends_with('\n') {
            ctx.out.push('\n');
        }
        ctx.pending_newline = false;
        ctx.pending_space = false;
    }
    if ctx.pending_space {
        ctx.pending_space = false;
        let last = ctx.out.chars().last();
        let first = s.chars().next();
        if is_ascii_word(last) && is_ascii_word(first) {
            ctx.out.push(' ');
        }
    }
    ctx.out.push_str(s);
}

fn emit_bytes(ctx: &mut Ctx, bytes: &[u8], font: Option<&Rc<FontInfo>>) {
    if bytes.is_empty() {
        return;
    }
    let s = match font {
        Some(f) => f.decode(bytes),
        None => fallback_decode(bytes),
    };
    if s.is_empty() {
        return;
    }
    // 同一位置、同样内容的重复绘制（伪粗体）直接丢掉
    if !ctx.pending_newline && !ctx.pending_space && ctx.is_duplicate_emit(&s) {
        return;
    }
    ctx.note_text_position();
    if !ctx.pending_newline {
        ctx.note_word_gap(&s);
    }
    ctx.last_text_width = estimate_text_width(&s, ctx.effective_font_size());
    ctx.remember_emit(&s);
    push_text(ctx, &s);
}

fn xobject_ref(res: &Val, name: &str) -> Option<u32> {
    let xo = v_get(res, b"XObject")?;
    let entries = match xo {
        Val::Dict(e) => e,
        _ => return None,
    };
    for (k, v) in entries {
        if k == name {
            return v_ref(v);
        }
    }
    None
}

/// 内容流渲染的「访问记录」。
///
/// Form XObject 允许被同一页使用多次（水印、图元、表单项都很常见），但不允许递归引用自己。
/// 旧代码只有一层深度上限：一个表单里放 50 次 `/X Do`、其中一次指向自己，
/// 深度 8 就是 50^8 次调用，而且每层调用的超时计数器都从 0 重新数，永远触发不了。
struct RenderGuard {
    /// 当前 `Do` 路径上的 Form 对象号（DFS 祖先链，出栈时移除）。
    ///
    /// 用「路径」而不是「全页 visited」：同一个表单在一页里画两次是合法 PDF，
    /// 只有出现在自己的祖先链上才是必须拦住的递归。
    xobject_stack: Vec<u32>,
    /// 这一页累计展开 Form 的次数（DAG 式的指数放大靠它收口）。
    xobject_calls: usize,
    /// 上限告警（同一类只记一行）。
    lim: LimitLog,
}

impl RenderGuard {
    fn new() -> RenderGuard {
        RenderGuard {
            xobject_stack: Vec::new(),
            xobject_calls: 0,
            lim: LimitLog::default(),
        }
    }

    /// 进入一次 `Do`；返回 `false` 表示这次展开应当跳过（已经记过日志）。
    fn enter_xobject(&mut self, num: u32) -> bool {
        if self.xobject_stack.contains(&num) {
            self.lim.hit(
                "xobject_recursion",
                &format!(
                    "Form XObject {} 在自己的调用链里再次出现（递归引用），已跳过",
                    num
                ),
            );
            return false;
        }
        if self.xobject_calls >= MAX_XOBJECT_CALLS {
            self.lim.hit(
                "xobject_calls",
                &format!(
                    "一页里的 Form XObject 展开次数超过上限 {}，剩余 /Do 已跳过",
                    MAX_XOBJECT_CALLS
                ),
            );
            return false;
        }
        self.xobject_stack.push(num);
        self.xobject_calls += 1;
        true
    }

    fn leave_xobject(&mut self) {
        self.xobject_stack.pop();
    }
}

fn render_content(
    ctx: &mut Ctx,
    guard: &mut RenderGuard,
    cmaps: &mut HashMap<u32, Rc<FontInfo>>,
    data: &[u8],
    fonts: &HashMap<String, Rc<FontInfo>>,
    res: &Val,
    depth: usize,
) {
    // 每次进入都查一次超时。递归下去的每一层都会把自己的计时从头开始数，
    // 旧代码只看「本层 token 数是不是 8192 的倍数」，嵌套一深就永远查不到超时。
    if Instant::now() > ctx.deadline {
        guard.lim.hit(
            "render_deadline",
            "渲染内容流超过总时间上限，剩余内容已放弃",
        );
        return;
    }
    if depth > MAX_FORM_DEPTH || data.is_empty() {
        return;
    }
    let toks = lex(data, false, true);
    let mut stack: Vec<Val> = Vec::new();
    let mut cur: Option<Rc<FontInfo>> = None;
    let mut counter: usize = 0;

    for t in &toks {
        counter += 1;
        if counter % 8192 == 0 && Instant::now() > ctx.deadline {
            return;
        }
        match t {
            Val::Op(op) => {
                match op.as_str() {
                    "BT" => {
                        // BT 重置文本矩阵
                        ctx.line_y = 0.0;
                        ctx.line_x = 0.0;
                        ctx.tm_scale = 1.0;
                        ctx.leading = 0.0;
                    }
                    "Tf" => {
                        if stack.len() >= 2 {
                            if let Val::Name(nm) = &stack[stack.len() - 2] {
                                cur = fonts.get(nm).cloned();
                            }
                        }
                        if let Some(sz) = stack_num(&stack, 1) {
                            ctx.font_size = sz;
                        }
                    }
                    "Tm" => {
                        // a b c d e f：d（倒数第 3 个）决定 y 缩放，f（倒数第 1 个）是行 y
                        if let Some(d) = stack_num(&stack, 3) {
                            let a = d.abs();
                            ctx.tm_scale = if a > 0.0 { a } else { 1.0 };
                        }
                        if let Some(f) = stack_num(&stack, 1) {
                            ctx.line_y = f;
                        }
                        // e（倒数第 2 个）是行 x
                        if let Some(e) = stack_num(&stack, 2) {
                            ctx.line_x = e;
                        }
                    }
                    "Td" | "TD" => {
                        // 只更新行 y；是否换行等真正输出文字时按 y 变化判断
                        if let Some(ty) = stack_num(&stack, 1) {
                            ctx.line_y += ty;
                            if op == "TD" {
                                ctx.leading = -ty;
                            }
                        }
                        // 横向位移先记下来：可能代表一个空格（见 note_word_gap）
                        let tx = match stack_num(&stack, 2) {
                            Some(v) => v,
                            None => 0.0,
                        };
                        ctx.line_x += tx;
                        if tx > 0.0 {
                            let scale = (ctx.tm_scale * ctx.ctm_scale).abs();
                            ctx.pending_x_advance = Some(tx * scale);
                        } else {
                            ctx.pending_x_advance = None;
                        }
                    }
                    "TL" => {
                        if let Some(l) = stack_num(&stack, 1) {
                            ctx.leading = l;
                        }
                    }
                    "T*" => {
                        // 规范里 T* 就是“移到下一行”
                        ctx.line_y -= ctx.leading;
                        ctx.force_newline();
                    }
                    "q" => {
                        ctx.ctm_stack.push(ctx.ctm_scale);
                    }
                    "Q" => {
                        if let Some(s) = ctx.ctm_stack.pop() {
                            ctx.ctm_scale = s;
                        }
                    }
                    "cm" => {
                        // a b c d e f：d 是倒数第 3 个
                        if let Some(d) = stack_num(&stack, 3) {
                            let a = d.abs();
                            if a > 0.0 {
                                ctx.ctm_scale *= a;
                            }
                        }
                    }
                    "Tj" => {
                        if let Some(Val::Str(s)) = stack.last() {
                            emit_bytes(ctx, s, cur.as_ref());
                        }
                    }
                    "TJ" => {
                        if let Some(Val::Array(items)) = stack.last() {
                            for it in items {
                                match it {
                                    Val::Str(s) => emit_bytes(ctx, s, cur.as_ref()),
                                    Val::Int(v) => {
                                        if *v <= -200 {
                                            ctx.pending_space = true;
                                        }
                                    }
                                    Val::Real(v) => {
                                        if *v <= -200.0 {
                                            ctx.pending_space = true;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "'" | "\"" => {
                        // 两者都先移到下一行再显示文字
                        ctx.line_y -= ctx.leading;
                        ctx.force_newline();
                        if let Some(Val::Str(s)) = stack.last() {
                            emit_bytes(ctx, s, cur.as_ref());
                        }
                    }
                    // ET 不直接换行：后面的 BT 一定会重新定位，按 y 变化来判断，
                    // 否则「一条基线上多个 BT/ET 文本段」的 PDF 会被拆成一行一个字。
                    "ET" => {}
                    "Do" => {
                        if depth < MAX_FORM_DEPTH {
                            if let Some(Val::Name(nm)) = stack.last() {
                                if let Some(xnum) = xobject_ref(res, nm) {
                                    // 先过 guard，再取流内容：被拦下的表单连一次字节拷贝都不做。
                                    if guard.enter_xobject(xnum) {
                                        let doc = ctx.doc;
                                        if let Some((bytes, r2)) = doc.form_parts(xnum, res) {
                                            let fonts2 = doc.build_font_map(&r2, cmaps);
                                            render_content(
                                                ctx,
                                                guard,
                                                cmaps,
                                                &bytes,
                                                &fonts2,
                                                &r2,
                                                depth + 1,
                                            );
                                        }
                                        guard.leave_xobject();
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                stack.clear();
            }
            other => {
                if stack.len() >= 64 {
                    stack.remove(0);
                }
                stack.push(other.clone());
            }
        }
    }
}

// ───────────────────────── 结果清理 ─────────────────────────

fn sanitize(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_space = false;

    for raw in s.chars() {
        let c = match raw {
            '\r' => '\n',
            '\t' => ' ',
            '\u{FEFF}' => continue,
            c => {
                let u = c as u32;
                if u < 0x20 && c != '\n' {
                    continue;
                }
                if (0x7F..0xA0).contains(&u) {
                    continue;
                }
                c
            }
        };
        if c == '\n' {
            lines.push(cur.trim_end().to_string());
            cur.clear();
            prev_space = false;
            continue;
        }
        if c == ' ' {
            if prev_space || cur.is_empty() {
                continue;
            }
            prev_space = true;
            cur.push(' ');
            continue;
        }
        prev_space = false;
        cur.push(c);
    }
    lines.push(cur.trim_end().to_string());

    let mut out = String::new();
    let mut blank: usize = 0;
    for l in &lines {
        if l.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank > 0 {
                out.push('\n');
            }
        }
        out.push_str(l);
        blank = 0;
    }
    out
}

fn looks_like_content(bytes: &[u8]) -> bool {
    find_keyword(bytes, 0, b"BT").is_some()
        || find_keyword(bytes, 0, b"Tj").is_some()
        || find_keyword(bytes, 0, b"TJ").is_some()
        || find_keyword(bytes, 0, b"Td").is_some()
}

// ───────────────────────── 测试 ─────────────────────────
//
// 全部 PDF 都用代码构造（本模块不依赖 xref，所以文件里的 xref 故意写成假的）。

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    // ── 构造工具 ──

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pdf_extract_lab_{}_{}_{}.pdf",
            tag,
            std::process::id(),
            nanos
        ));
        p
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = tmp_path(tag);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_file(p);
    }

    /// 拼一个 PDF：对象体 + 假 trailer（本模块不解析 xref）。
    fn build_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
        let numbered: Vec<(u32, Vec<u8>)> = objects
            .iter()
            .enumerate()
            .map(|(i, b)| (i as u32 + 1, b.clone()))
            .collect();
        build_pdf_numbered(&numbered)
    }

    /// 同上，但对象号自己指定（对象流测试要用）。
    fn build_pdf_numbered(objects: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"%PDF-1.6\n%\xE2\xE3\xCF\xD3\n");
        for (num, body) in objects {
            out.extend_from_slice(format!("{} 0 obj\n", num).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        out.extend_from_slice(b"trailer\n<< /Size 99 /Root 1 0 R >>\nstartxref\n0\n%%EOF\n");
        out
    }

    /// ObjStm 的明文负载：头部是若干组 `对象号 偏移`（偏移相对 /First），后面是对象体。
    /// 返回（负载, first）。B26 的 PNG 预测器测试要自己编码这段明文，所以单独抽出来。
    fn objstm_payload(objs: &[(u32, Vec<u8>)]) -> (Vec<u8>, usize) {
        let mut header = String::new();
        let mut bodies: Vec<u8> = Vec::new();
        for (num, body) in objs {
            header.push_str(&format!("{} {} ", num, bodies.len()));
            bodies.extend_from_slice(body);
            bodies.push(b' ');
        }
        let first = header.len();
        let mut payload: Vec<u8> = header.into_bytes();
        payload.extend_from_slice(&bodies);
        (payload, first)
    }

    /// 把一批对象打包成 `/Type /ObjStm` 对象流（FlateDecode 压缩）。
    fn objstm_obj(objs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let (payload, first) = objstm_payload(objs);
        let packed = zlib_bytes(&payload);
        stream_obj(
            &format!(" /Type /ObjStm /N {} /First {} /Filter /FlateDecode", objs.len(), first),
            &packed,
        )
    }

    fn stream_obj(extra: &str, data: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(format!("<< /Length {}{} >>\nstream\n", data.len(), extra).as_bytes());
        v.extend_from_slice(data);
        v.extend_from_slice(b"\nendstream");
        v
    }

    fn zlib_bytes(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn simple_font() -> Vec<u8> {
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec()
    }

    fn cid_font(to_unicode_obj: u32) -> Vec<u8> {
        format!(
            "<< /Type /Font /Subtype /Type0 /BaseFont /TestCID /Encoding /Identity-H \
             /DescendantFonts [99 0 R] /ToUnicode {} 0 R >>",
            to_unicode_obj
        )
        .into_bytes()
    }

    fn page_obj(font: u32, contents: u32) -> Vec<u8> {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 {} 0 R >> >> /Contents {} 0 R >>",
            font, contents
        )
        .into_bytes()
    }

    /// 标准单页文档：1 Catalog / 2 Pages / 3 Page / 4 Font / 5 Contents / 6.. extra
    fn std_pdf(font_obj: Vec<u8>, content: &[u8], compress: bool, extra: Vec<Vec<u8>>) -> Vec<u8> {
        let (data, filt) = if compress {
            (zlib_bytes(content), " /Filter /FlateDecode")
        } else {
            (content.to_vec(), "")
        };
        let mut objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            page_obj(4, 5),
            font_obj,
            stream_obj(filt, &data),
        ];
        objs.extend(extra);
        build_pdf(&objs)
    }

    fn cjk_bfchar_cmap() -> Vec<u8> {
        b"/CIDInit /ProcSet findresource begin\n\
12 dict begin\n\
begincmap\n\
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
/CMapName /Adobe-Identity-UCS def\n\
/CMapType 2 def\n\
1 begincodespacerange\n\
<0000> <FFFF>\n\
endcodespacerange\n\
2 beginbfchar\n\
<0001> <4F60>\n\
<0002> <597D>\n\
endbfchar\n\
endcmap\n\
CMapName currentdict /CMap defineresource pop\n\
end\n\
end\n"
            .to_vec()
    }

    // ── 1. 最简：未压缩内容流 + 字面字符串 ──

    #[test]
    fn t01_minimal_uncompressed_hello_world() {
        let pdf = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 72 720 Td (Hello World) Tj ET",
            false,
            vec![],
        );
        let p = write_tmp("min", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert!(got.contains("Hello World"), "实际抽出: {:?}", got);
        assert_eq!(got, "Hello World");
    }

    // ── 2. FlateDecode（zlib 包装） ──

    #[test]
    fn t02_flate_content_stream() {
        // 用高度可压缩的长串，保证 zlib 不会退化成 stored 块（短文本时会）。
        let text = "A".repeat(40);
        let content = format!("BT /F1 12 Tf 72 720 Td ({}) Tj ET", text);
        let pdf = std_pdf(simple_font(), content.as_bytes(), true, vec![]);
        // 文件里必须标记 FlateDecode，且明文不能原样出现（说明确实压缩了、也确实解开了）
        assert!(find_sub(&pdf, b"/FlateDecode").is_some());
        assert!(find_sub(&pdf, b"AAAAAAAAAA").is_none());
        let p = write_tmp("flate", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, text);
    }

    // ── 3. 中文：bfchar + ToUnicode 十六进制串 ──

    #[test]
    fn t03_cjk_bfchar_to_unicode() {
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 72 720 Td <00010002> Tj ET",
            false,
            vec![stream_obj("", &cjk_bfchar_cmap())],
        );
        let p = write_tmp("cjk_bfchar", &pdf);
        let res = extract_pdf_text(&p);
        cleanup(&p);
        let got = res.unwrap();
        println!("[t03] 实际抽出: {:?}", got);
        assert_eq!(got, "你好", "中文没抽对: {:?}", got);
        assert!(got.contains('你') && got.contains('好'));
    }

    // ── 4. bfrange（连续码位，含数值自增） ──

    #[test]
    fn t04_cjk_bfrange_sequential() {
        let cmap = b"begincmap\n\
1 begincodespacerange\n\
<0000> <FFFF>\n\
endcodespacerange\n\
1 beginbfrange\n\
<0020> <0022> <0041>\n\
endbfrange\n\
endcmap\n"
            .to_vec();
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 72 720 Td <002000210022> Tj ET",
            false,
            vec![stream_obj("", &cmap)],
        );
        let p = write_tmp("bfrange", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "ABC");
    }

    // ── 5. bfrange 数组形式 ──

    #[test]
    fn t05_bfrange_array_form() {
        let cmap = b"1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
1 beginbfrange\n\
<0001> <0003> [<4E00> <4E01> <4E02>]\n\
endbfrange\n"
            .to_vec();
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 72 720 Td <000100020003> Tj ET",
            false,
            vec![stream_obj("", &cmap)],
        );
        let p = write_tmp("bfrange_arr", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "一丁丂", "实际: {:?}", got);
    }

    // ── 6. 代理对（非 BMP）──

    #[test]
    fn t06_bfchar_surrogate_pair() {
        // U+1F600 🙂 = D83D DE00
        let cmap = b"1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n\
1 beginbfchar\n<0001> <D83DDE00>\nendbfchar\n"
            .to_vec();
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 20 720 Td <0001> Tj ET",
            false,
            vec![stream_obj("", &cmap)],
        );
        let p = write_tmp("surrogate", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "\u{1F600}");
    }

    // ── 7. 转义与括号嵌套 ──

    #[test]
    fn t07_string_escapes() {
        let cases: Vec<(&[u8], &str)> = vec![
            (b"BT /F1 12 Tf 10 700 Td (a\\(b\\)c) Tj ET", "a(b)c"),
            (b"BT /F1 12 Tf 10 700 Td (\\050) Tj ET", "("),
            (b"BT /F1 12 Tf 10 700 Td (x\\055y) Tj ET", "x-y"),
            (b"BT /F1 12 Tf 10 700 Td (a\\\\b) Tj ET", "a\\b"),
            (b"BT /F1 12 Tf 10 700 Td (a(b)c) Tj ET", "a(b)c"),
            (b"BT /F1 12 Tf 10 700 Td (tab\\there) Tj ET", "tab here"),
        ];
        for (content, want) in cases {
            let pdf = std_pdf(simple_font(), content, false, vec![]);
            let p = write_tmp("escape", &pdf);
            let got = extract_pdf_text(&p).unwrap();
            cleanup(&p);
            assert_eq!(got, want, "内容 {:?}", String::from_utf8_lossy(content));
        }
    }

    // ── 8. 十六进制字符串 + TJ 数组 + 换行算子 ──

    #[test]
    fn t08_tj_array_and_newlines() {
        let pdf = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (A) Tj T* (B) Tj 1 1 (C) \" ET",
            false,
            vec![],
        );
        let p = write_tmp("newline", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "A\nB\nC", "实际: {:?}", got);

        // hex + TJ 数组：小间距不加空格，大间距加空格
        let pdf2 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td <48656C6C6F> Tj ET",
            false,
            vec![],
        );
        let p2 = write_tmp("hex", &pdf2);
        let got2 = extract_pdf_text(&p2).unwrap();
        cleanup(&p2);
        assert_eq!(got2, "Hello");

        let pdf3 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td [(He) -50 (llo)] TJ ET",
            false,
            vec![],
        );
        let p3 = write_tmp("tj", &pdf3);
        let got3 = extract_pdf_text(&p3).unwrap();
        cleanup(&p3);
        assert_eq!(got3, "Hello");

        let pdf4 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td [(He) -1000 (llo)] TJ ET",
            false,
            vec![],
        );
        let p4 = write_tmp("tj_space", &pdf4);
        let got4 = extract_pdf_text(&p4).unwrap();
        cleanup(&p4);
        assert_eq!(got4, "He llo");
    }

    // ── 9. Td / TD 换行 ──

    #[test]
    fn t09_td_inserts_newline() {
        let pdf = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (line1) Tj 10 660 Td (line2) Tj ET",
            false,
            vec![],
        );
        let p = write_tmp("td", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "line1\nline2", "实际: {:?}", got);
    }

    // ── 10. 多页，顺序正确且不重复 ──

    #[test]
    fn t10_multi_page_order() {
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>".to_vec(),
            page_obj(4, 5),
            simple_font(),
            stream_obj("", b"BT /F1 12 Tf 10 700 Td (PAGE-ONE) Tj ET"),
            page_obj(4, 7),
            stream_obj("", b"BT /F1 12 Tf 10 700 Td (PAGE-TWO) Tj ET"),
        ];
        let pdf = build_pdf(&objs);
        let p = write_tmp("pages", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "PAGE-ONE\nPAGE-TWO", "实际: {:?}", got);
    }

    // ── 11. 流前后 EOL 的处理（CRLF、以及没有 /Length 的情况） ──

    #[test]
    fn t11_stream_boundaries_crlf_and_no_length() {
        // CRLF 版：手写对象，stream 后是 \r\n，数据后是 \r\n
        let mut objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            page_obj(4, 5),
            simple_font(),
        ];
        let content = b"BT /F1 12 Tf 10 700 Td (CRLF-Works) Tj ET";
        let mut so: Vec<u8> = Vec::new();
        so.extend_from_slice(format!("<< /Length {} >>\r\nstream\r\n", content.len()).as_bytes());
        so.extend_from_slice(content);
        so.extend_from_slice(b"\r\nendstream");
        objs.push(so);
        let pdf = build_pdf(&objs);
        let p = write_tmp("crlf", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "CRLF-Works");

        // 没有 /Length：必须靠 endstream 定位，且结尾 EOL 不算数据
        let content2 = b"BT /F1 12 Tf 10 700 Td (No-Length-Works) Tj ET";
        let mut objs2: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            page_obj(4, 5),
            simple_font(),
        ];
        let mut so2: Vec<u8> = Vec::new();
        so2.extend_from_slice(b"<< /Type /Page >>\nstream\n");
        so2.extend_from_slice(content2);
        so2.extend_from_slice(b"\nendstream");
        objs2.push(so2);
        let pdf2 = build_pdf(&objs2);
        let p2 = write_tmp("nolen", &pdf2);
        let got2 = extract_pdf_text(&p2).unwrap();
        cleanup(&p2);
        assert_eq!(got2, "No-Length-Works");
    }

    // ── 12. Form XObject 递归 ──

    #[test]
    fn t12_form_xobject_recursion() {
        // 3 Page 的 Resources 里带 /XObject << /Fm1 6 0 R >>，内容流用 /Fm1 Do
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
               /Resources << /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >> \
               /Contents 5 0 R >>"
                .to_vec(),
            simple_font(),
            stream_obj("", b"BT /F1 12 Tf 10 700 Td (Before Form) Tj ET /Fm1 Do"),
            {
                let data = b"BT /F1 12 Tf 10 600 Td (Inside Form) Tj ET";
                let mut v: Vec<u8> = Vec::new();
                v.extend_from_slice(
                    format!(
                        "<< /Length {} /Subtype /Form /Type /XObject >>\nstream\n",
                        data.len()
                    )
                    .as_bytes(),
                );
                v.extend_from_slice(data);
                v.extend_from_slice(b"\nendstream");
                v
            },
        ];
        let pdf = build_pdf(&objs);
        let p = write_tmp("form", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert!(got.contains("Before Form"), "实际: {:?}", got);
        assert!(got.contains("Inside Form"), "实际: {:?}", got);
    }

    // ── 13. 畸形输入绝不 panic ──

    #[test]
    fn t13_malformed_inputs_do_not_panic() {
        // 空文件
        let p = write_tmp("empty", b"");
        let r = extract_pdf_text(&p);
        cleanup(&p);
        assert!(r.is_err());

        // 纯随机字节
        let mut rnd: Vec<u8> = Vec::new();
        let mut seed: u32 = 0x1234_5678;
        for _ in 0..4096 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            rnd.push((seed >> 16) as u8);
        }
        let p = write_tmp("random", &rnd);
        let r = extract_pdf_text(&p);
        cleanup(&p);
        assert!(r.is_err());

        // 有 %PDF- 头后面全是垃圾
        let mut junk: Vec<u8> = b"%PDF-1.4\n".to_vec();
        for i in 0..2000u32 {
            junk.push((i % 251) as u8);
            if i % 97 == 0 {
                junk.extend_from_slice(b" 1 0 obj << /Type /Page");
            }
            if i % 53 == 0 {
                junk.extend_from_slice(b"stream\n(broken");
            }
        }
        let p = write_tmp("junk", &junk);
        let _ = extract_pdf_text(&p);
        cleanup(&p);

        // 截断：把好的 PDF 砍成好几段
        let good = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (Truncated) Tj ET",
            false,
            vec![],
        );
        for cut in [1usize, 10, 40, good.len() / 3, good.len() / 2, good.len() - 1] {
            let part = &good[..cut.min(good.len())];
            let p = write_tmp("trunc", part);
            let _ = extract_pdf_text(&p);
            cleanup(&p);
        }

        // 只有一个 obj 头，没有 endobj
        let p = write_tmp("noendobj", b"%PDF-1.4\n1 0 obj << /Length 3 >>\nstream\nabc");
        let _ = extract_pdf_text(&p);
        cleanup(&p);

        // 坏 zlib：声明 FlateDecode 但数据是垃圾
        let bad = std_pdf(
            simple_font(),
            b"BT (never visible) Tj ET",
            true,
            vec![],
        );
        let mut mutated = bad.clone();
        // 破坏压缩流中间的一些字节
        let mid = mutated.len() / 2;
        for k in 0..16 {
            if mid + k < mutated.len() {
                mutated[mid + k] ^= 0xFF;
            }
        }
        let p = write_tmp("badzlib", &mutated);
        let _ = extract_pdf_text(&p);
        cleanup(&p);

        // 路径不存在 / 是目录
        let missing = tmp_path("does_not_exist");
        assert!(extract_pdf_text(&missing).is_err());
        assert!(extract_pdf_text(std::path::Path::new(".")).is_err());
        // 非 PDF 的文本文件
        let p = write_tmp("notpdf", b"hello, I am definitely not a pdf");
        assert!(extract_pdf_text(&p).is_err());
        cleanup(&p);
    }

    // ── 14. 抽不到文字时的中文错误 ──

    #[test]
    fn t14_empty_text_returns_error() {
        let pdf = std_pdf(simple_font(), b"BT /F1 12 Tf 10 700 Td ET", false, vec![]);
        let p = write_tmp("notext", &pdf);
        let r = extract_pdf_text(&p);
        cleanup(&p);
        match r {
            Err(e) => assert!(e.contains("没抽到文字"), "错误信息: {}", e),
            Ok(t) => panic!("不该成功: {:?}", t),
        }
    }

    // ── 15. 加密标记给出明确错误 ──

    #[test]
    fn t15_encrypted_hint() {
        let mut objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            page_obj(4, 5),
            simple_font(),
        ];
        objs.push(stream_obj("", b"BT /F1 12 Tf 10 700 Td ET"));
        let mut pdf = build_pdf(&objs);
        pdf.extend_from_slice(b"trailer << /Encrypt 9 0 R >>\n");
        let p = write_tmp("enc", &pdf);
        let r = extract_pdf_text(&p);
        cleanup(&p);
        match r {
            Err(e) => assert!(e.contains("加密"), "错误信息: {}", e),
            Ok(t) => panic!("不该成功: {:?}", t),
        }
    }

    // ── 16. 控制字符与空白折叠 ──

    #[test]
    fn t16_sanitize_controls_and_blank_lines() {
        // 控制字符被丢掉，连续空格折叠；连续的换行请求（T* T* T*）折叠成一个换行
        let pdf = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (A\x01B   C) Tj T* T* T* (D\x02) Tj ET",
            false,
            vec![],
        );
        let p = write_tmp("sanitize", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "AB C\nD", "实际: {:?}", got);

        // 字符串里的多个换行：3 个以上连续空行折叠成 1 个空行
        let pdf2 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (X\n\n\n\nY) Tj ET",
            false,
            vec![],
        );
        let p2 = write_tmp("sanitize2", &pdf2);
        let got2 = extract_pdf_text(&p2).unwrap();
        cleanup(&p2);
        assert_eq!(got2, "X\n\nY", "实际: {:?}", got2);
    }

    // ── 17. 无 ToUnicode 的 Type0 字体走 UTF-16BE ──

    #[test]
    fn t17_type0_without_tounicode_utf16be() {
        let font = b"<< /Type /Font /Subtype /Type0 /BaseFont /NoTU /Encoding /Identity-H >>".to_vec();
        let pdf = std_pdf(
            font,
            b"BT /F1 12 Tf 10 700 Td <4F60597D> Tj ET",
            false,
            vec![],
        );
        let p = write_tmp("notu", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "你好", "实际: {:?}", got);
    }

    // ── 18. 简单字体无 ToUnicode：ASCII 不能被 UTF-16BE 误判 ──

    #[test]
    fn t18_simple_font_ascii_not_mangled() {
        let pdf = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (Test) Tj ET",
            false,
            vec![],
        );
        let p = write_tmp("ascii4", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "Test", "实际: {:?}", got);
    }

    // ── 19. 换行判断的两种真实形态 ──

    #[test]
    fn t20_line_break_detection() {
        // (a) WPS / 某些排版器导出：同一条基线上逐字符 `TD ... -0`（ty = 0），
        //     绝不能拆成一行一个字。
        let content = b"BT /F1 480 Tf 0.05 0 0 -0.05 87.959 150.17 Tm (He) Tj 482.391 -0 TD (llo) Tj ET \
BT /F1 480 Tf 0.05 0 0 -0.05 87.959 174.17 Tm (World) Tj ET";
        let pdf = std_pdf(simple_font(), content, false, vec![]);
        let p = write_tmp("td_zero", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "Hello\nWorld", "实际: {:?}", got);

        // (b) TD 真的换了行 → 断行
        let pdf2 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (l1) Tj 0 -14 TD (l2) Tj ET",
            false,
            vec![],
        );
        let p2 = write_tmp("td_move", &pdf2);
        let got2 = extract_pdf_text(&p2).unwrap();
        cleanup(&p2);
        assert_eq!(got2, "l1\nl2", "实际: {:?}", got2);

        // (c) 同一行里只横向移动 → 不断行；位移 ≈ 字宽（逐字符定位）→ 也不加空格
        let pdf3 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (A) Tj 8 0 Td (B) Tj ET",
            false,
            vec![],
        );
        let p3 = write_tmp("td_x", &pdf3);
        let got3 = extract_pdf_text(&p3).unwrap();
        cleanup(&p3);
        assert_eq!(got3, "AB", "实际: {:?}", got3);

        // (d) 位移远大于字宽 → 视为词间空格，但依然不断行
        let pdf4 = std_pdf(
            simple_font(),
            b"BT /F1 12 Tf 10 700 Td (A) Tj 20 0 Td (B) Tj ET",
            false,
            vec![],
        );
        let p4 = write_tmp("td_gap", &pdf4);
        let got4 = extract_pdf_text(&p4).unwrap();
        cleanup(&p4);
        assert_eq!(got4, "A B", "实际: {:?}", got4);
    }

    // ── 20. 空格被编码成 Td 横向位移时，要能还原词间空格 ──

    #[test]
    fn t21_word_gap_via_td_advance() {
        // 印刷驱动（Kyocera 等）常见形态：不用空格字符，用一个横向 Td 表示词距。
        // 数字取自真实 PDF：14pt 的 "KX Driver Upgrades in Client-Server Environments"
        let content = b"BT /F1 14 Tf 100 Tz 56.7 772.7 Td (KX)Tj 22.93 0 Td (Driver)Tj \
43.95 0 Td (Upgrades)Tj 68.03 0 Td (in)Tj 15.92 0 Td (Client-Server)Tj 90.58 0 Td (Environments)Tj ET";
        let pdf = std_pdf(simple_font(), content, false, vec![]);
        let p = write_tmp("wordgap", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(
            got, "KX Driver Upgrades in Client-Server Environments",
            "实际: {:?}",
            got
        );

        // 逐字符横向定位（位移 ≈ 字宽）不能被塞进空格
        let content2 = b"BT /F1 10 Tf 170.1 733.4 Td (In)Tj 8.34 0 Td (a)Tj 5.56 0 Td (row)Tj ET";
        let pdf2 = std_pdf(simple_font(), content2, false, vec![]);
        let p2 = write_tmp("nogap", &pdf2);
        let got2 = extract_pdf_text(&p2).unwrap();
        cleanup(&p2);
        assert_eq!(got2, "Inarow", "实际: {:?}", got2);
    }

    // ── 21. 对象流（/Type /ObjStm）：页面树与字体字典都被压缩进对象流 ──

    #[test]
    fn t22_object_stream_expansion() {
        let contents = stream_obj("", b"BT /F1 12 Tf 72 720 Td <00010002> Tj ET");
        let cmap = stream_obj("", &cjk_bfchar_cmap());
        let objstm = objstm_obj(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (3, page_obj(4, 5)),
            (4, cid_font(6)),
        ]);
        let pdf = build_pdf_numbered(&[
            (5, contents),
            (6, cmap),
            (7, objstm),
        ]);
        // 明文里没有页面树字典，确实只能靠对象流拿到
        assert!(find_sub(&pdf, b"/Type /Catalog").is_none());
        let p = write_tmp("objstm", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "你好", "对象流里的 ToUnicode 没生效: {:?}", got);
    }

    // ── 22. 可选：真实文件冒烟（默认忽略） ──

    #[test]
    #[ignore]
    fn t19_smoke_real_pdf_from_env() {
        let path = match std::env::var("PDF_TEST_FILE") {
            Ok(p) => p,
            Err(_) => {
                println!("未设置 PDF_TEST_FILE，跳过");
                return;
            }
        };
        // 先打印一行诊断信息，方便排查“抽出来是乱码”的原因
        if let Ok(data) = std::fs::read(&path) {
            let doc = Doc::build(&data, Instant::now() + TOTAL_TIMEOUT);
            let mut lim = LimitLog::default();
            let pages = doc.page_order(&mut lim);
            println!(
                "[diag] 对象 {} 个，可解码流 {} 个，页面 {} 个，对象流 {} 个",
                doc.order.len(),
                doc.streams.values().filter(|s| s.is_some()).count(),
                pages.len(),
                doc.order.iter().filter(|n| doc.is_type(**n, "ObjStm")).count()
            );
            for p in pages.iter().take(3) {
                if let Some(d) = doc.vals.get(p) {
                    if let Some(res) = doc.inherited_resources(d, 0) {
                        if let Some(Val::Dict(fonts)) = v_get(&res, b"Font") {
                            for (fname, fv) in fonts {
                                if let Some(fnum) = v_ref(fv) {
                                    let sub = match doc.vals.get(&fnum) {
                                        Some(fd) => v_name_or(v_get(fd, b"Subtype"), "?"),
                                        None => "?",
                                    };
                                    let has_tu = doc.load_cmap(fnum).is_some();
                                    println!(
                                        "[diag] 页 {} 字体 {} -> obj {} subtype={} ToUnicode={}",
                                        p, fname, fnum, sub, has_tu
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        match extract_pdf_text(std::path::Path::new(&path)) {
            Ok(t) => {
                let head: String = t.chars().take(500).collect();
                println!("[smoke] 共 {} 字符，前 500 字：\n{}", t.chars().count(), head);
                assert!(!t.is_empty());
            }
            Err(e) => panic!("[smoke] 抽取失败: {}", e),
        }
    }

    // ── 23. S5 / B26 / B27：加固（每条都用构造出来的真实攻击输入，不信“读代码推理”） ──
    //
    // 这里每个恶意用例都用 Instant 量一个宽松的上界：加固前这些输入要么挂死（递归爆炸、
    // 空转几十亿次），要么把内存吃光（进程被系统 kill，测试连报错都留不下），
    // 所以「能在有限时间内返回」本身就是断言的一部分。

    /// 恶意用例的时间上界。文件都很小，正常路径是毫秒级；
    /// 2 秒是留给 debug 构建（cargo test 默认不优化）和慢机器的余量。
    const MALICIOUS_TIME_LIMIT: Duration = Duration::from_secs(2);

    /// 需要真的产出到单流上限（64 MB）的用例的上界。
    ///
    /// debug 构建下光是「一个字节一个字节地拼出 64 MB」就要几秒，这条上界只用来兜住
    /// 「永远不返回 / 内存爆掉」，不是性能指标。
    const HEAVY_TIME_LIMIT: Duration = Duration::from_secs(8);

    /// `/Contents` 数组重复引用同一个流的攻击文件：一段真实内容 + 4 MB 空白。
    ///
    /// 空白是故意的：词法分析遇到空白只往前走、不产生 token，
    /// 这样测试能撑爆拼接预算，又不会把测试本身变成「解码 32 MB 文本」的压测。
    fn contents_repeat_bomb_pdf(refs: usize, tail: usize) -> Vec<u8> {
        let mut chunk: Vec<u8> = b"BT /F1 12 Tf 10 700 Td (A) Tj ET".to_vec();
        chunk.extend(std::iter::repeat(b' ').take(tail));
        let mut kids = String::from("[");
        for _ in 0..refs {
            kids.push_str("5 0 R ");
        }
        kids.push(']');
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 4 0 R >> >> /Contents {} >>",
                kids
            )
            .into_bytes(),
            simple_font(),
            stream_obj("", &chunk),
        ];
        build_pdf(&objs)
    }

    // ── S5-1：自引用的页面树 ──

    #[test]
    fn s5_1_self_referencing_page_tree_terminates() {
        // 报告里的原样攻击：节点 2 的 /Kids 里 4 次引用它自己。
        // 旧代码没有 visited（而且 out 只在遇到 /Type /Page 时才增长，MAX_PAGES 根本拦不住），
        // 深度 32 下要做约 4^32 次调用 —— 线程被永久占满 CPU，这个测试会挂死。
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [2 0 R 2 0 R 2 0 R 2 0 R] /Count 4 >>".to_vec(),
        ];
        let p = write_tmp("s5_pagetree", &build_pdf(&objs));
        let start = Instant::now();
        let r = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_1] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "自引用页面树没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
        // 树里一个 /Type /Page 都没有 → 应该是「正常失败」（明确的中文错误），而不是挂死
        assert!(r.is_err(), "自引用页面树不该抽出正文: {:?}", r);
    }

    // ── S5-2：自嵌套的 Form XObject ──

    #[test]
    fn s5_2_self_nesting_form_xobject_terminates() {
        // 每个表单里有 50 次 /Fm1 Do，而 Fm1 就是它自己。
        // 旧代码只有 8 层深度上限：50^8 次调用跑不完；而且每层递归的超时计数器都从 0 重新数，
        // 8192 的倍数永远等不到。这个测试在加固前是「跑到超时 / 卡死」。
        let mut form_content: Vec<u8> = b"BT /F1 12 Tf 10 600 Td (Form) Tj ET ".to_vec();
        let mut page_content: Vec<u8> = b"BT /F1 12 Tf 10 700 Td (Page) Tj ET ".to_vec();
        for _ in 0..50 {
            form_content.extend_from_slice(b"/Fm1 Do ");
            page_content.extend_from_slice(b"/Fm1 Do ");
        }
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
               /Resources << /Font << /F1 4 0 R >> /XObject << /Fm1 6 0 R >> >> \
               /Contents 5 0 R >>"
                .to_vec(),
            simple_font(),
            stream_obj("", &page_content),
            // 表单故意不写 /Resources：按 PDF 规则继承页面资源，于是 /Fm1 又指向它自己
            stream_obj(" /Subtype /Form /Type /XObject", &form_content),
        ];
        let p = write_tmp("s5_xobject", &build_pdf(&objs));
        let start = Instant::now();
        let r = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_2] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "自嵌套表单没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
        let got = r.unwrap();
        // 加固只砍「递归」，不能把合法的第一次渲染也砍掉（表单正文必须还在）
        assert!(got.contains("Page"), "页面正文丢了: {:?}", got);
        assert!(got.contains("Form"), "表单正文丢了: {:?}", got);
    }

    // ── S5-3：/Contents 数组重复引用 ──

    #[test]
    fn s5_3_contents_array_repeat_reference_is_capped() {
        // 一个 4 MB 的流被 /Contents 引用 64 次：不设上限就是 256 MB 的拼接结果
        //（真实攻击是「解压后 63 MB 的炸弹 × 500 次 ≈ 32 GB」）。
        // 这里直接查拼接结果本身，避免端到端测试把 32 MB 全解码一遍。
        let pdf = contents_repeat_bomb_pdf(64, 4 * 1024 * 1024);
        let doc = Doc::build(&pdf, Instant::now() + TOTAL_TIMEOUT);
        let page = *doc
            .vals
            .keys()
            .find(|n| doc.is_type(**n, "Page"))
            .expect("攻击文件里必须有页面对象");
        let d = doc.vals.get(&page).unwrap();
        let mut lim = LimitLog::default();
        let start = Instant::now();
        let content = doc.collect_contents(d, &mut lim);
        let spent = start.elapsed();
        println!("[s5_3] 拼接 {} 字节，耗时 {:?}", content.len(), spent);
        assert!(
            content.len() <= MAX_CONTENT_BYTES,
            "拼接结果 {} 字节，超过 {} 字节上限",
            content.len(),
            MAX_CONTENT_BYTES
        );
        // 上限真的起了作用：没有把 64 × 4 MB 全拼进去
        assert!(
            content.len() >= MAX_CONTENT_BYTES / 2,
            "预算远没用到就停了（只拼了 {} 字节），说明截断位置不对",
            content.len()
        );
    }

    #[test]
    fn s5_3_contents_bomb_returns_in_time() {
        // 端到端：截断后仍要走完取字、渲染，确认不会挂死、也不会把 256 MB 吃进内存。
        let p = write_tmp("s5_contents", &contents_repeat_bomb_pdf(64, 4 * 1024 * 1024));
        let start = Instant::now();
        let r = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_3-e2e] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "/Contents 炸弹没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
        assert!(r.unwrap().contains('A'), "被截断后连第一段正文都丢了");
    }

    #[test]
    fn s5_3_multiple_content_streams_still_render() {
        // 反向保护：正常的多内容流页面（/Contents [5 0 R 6 0 R]）必须照旧拼成一行，
        // 不能被新的字节预算误伤。
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
               /Resources << /Font << /F1 4 0 R >> >> /Contents [5 0 R 6 0 R] >>"
                .to_vec(),
            simple_font(),
            stream_obj("", b"BT /F1 12 Tf 10 700 Td (Hello) Tj ET"),
            stream_obj("", b"BT /F1 12 Tf 60 700 Td (World) Tj ET"),
        ];
        let p = write_tmp("s5_multicontent", &build_pdf(&objs));
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert!(got.contains("Hello"), "第一段内容流丢了: {:?}", got);
        assert!(got.contains("World"), "第二段内容流丢了: {:?}", got);
        assert!(!got.contains('\n'), "同一个 y 上的两段内容流被拆成了两行: {:?}", got);
    }

    // ── S5-4：解压总预算 / ascii85 的 z 炸弹 ──

    #[test]
    fn s5_4_total_decompressed_budget_is_enforced() {
        // 8 个「小压缩炸弹」：每个解压出 1 MB 全 0（zlib 压缩后只有几百字节），单个都合法。
        // 真实攻击是几百个这样的流（每个上限 64 MB），加起来几十 GB，直接 OOM 整个进程。
        // 这里把预算调小到 3 MB 走同一条代码路径：不做小预算的话测试本身就要解压 128 MB。
        let budget = 3 * 1024 * 1024;
        let mut objs: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..8u32 {
            let payload = vec![0u8; 1024 * 1024];
            objs.push((
                10 + i,
                stream_obj(" /Filter /FlateDecode", &zlib_bytes(&payload)),
            ));
        }
        let pdf = build_pdf_numbered(&objs);
        let start = Instant::now();
        let doc = Doc::build_with_budget(&pdf, Instant::now() + TOTAL_TIMEOUT, budget);
        let spent = start.elapsed();
        let total: usize = doc
            .streams
            .values()
            .filter_map(|s| s.as_ref())
            .map(|b| b.len())
            .sum();
        println!("[s5_4] 预算 {} 字节，实际驻留 {} 字节，耗时 {:?}", budget, total, spent);
        assert!(
            total <= budget,
            "解压总量 {} 字节超过预算 {} 字节",
            total,
            budget
        );
        assert!(
            total >= 1024 * 1024,
            "预算没被用上（只驻留 {} 字节），说明流根本没解压",
            total
        );
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "解压炸弹没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
    }

    #[test]
    fn s5_4_ascii85_z_is_capped() {
        // ascii85 的 `z` 一个字符膨胀成 4 个 0 字节，而且不走 5 字节分组那条路径 ——
        // 旧代码只挡住了分组路径，`z` 能无限膨胀（几百 MB 输入 → GB 级 Vec）。
        // 撞到单流 64 MB 上限需要 16 M 个 `z`。
        let mut z: Vec<u8> = vec![b'z'; MAX_STREAM_OUT / 4 + 16];
        let mut lim = LimitLog::default();
        let start = Instant::now();
        let out = ascii85_decode(&z, &mut lim);
        let spent = start.elapsed();
        z.clear();
        println!("[s5_4-z] 耗时 {:?}", spent);
        assert!(out.is_none(), "ascii85 的 z 炸弹没有被单流上限拦住");
        assert!(
            spent < HEAVY_TIME_LIMIT,
            "ascii85 z 炸弹没有在 {:?} 内返回（实际 {:?}）",
            HEAVY_TIME_LIMIT,
            spent
        );
    }

    // ── S5-5：bfrange 的超长目标串 ──

    #[test]
    fn s5_5_bfrange_oversized_target_does_not_spin() {
        // 报告里的原样攻击：目标串 5 字节。旧代码里 u64_to_utf16be_string 恒返回 None、
        // map 永远写不进去、entries 也不增长，<00000000> <FFFFFFFF> 就是约 43 亿次空转。
        let cmap = b"1 begincodespacerange\n<00000000> <FFFFFFFF>\nendcodespacerange\n\
1 beginbfrange\n<00000000> <FFFFFFFF> <0000000000>\nendbfrange\n"
            .to_vec();
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 72 720 Td <000000000001> Tj ET",
            false,
            vec![stream_obj("", &cmap)],
        );
        let p = write_tmp("s5_bfrange", &pdf);
        let start = Instant::now();
        let r = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_5] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "超长 bfrange 目标没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
        // 这个区间被整段跳过 → CMap 一条映射都没有 → 抽不到正文（但也绝不能挂死）
        assert!(r.is_err(), "非法 bfrange 不该抽出正文: {:?}", r);
    }

    // ── S5-6：缺 endobj / 缺 endstream 的扫描 ──

    #[test]
    fn s5_6_objects_without_endobj_stay_bounded() {
        // 「有 obj 头、既没 endobj 也没 stream」的对象，彼此间隔 1 MB。
        // 旧代码每个都从对象头扫到文件尾：n 个对象就是 n²，200 MB 的文件能稳定耗满 60 秒超时。
        // 这里缩到 8 MB 的规模（测试不该为了复现 60 秒而造 200 MB 文件），断言的是「有上界」。
        let mut pdf: Vec<u8> = b"%PDF-1.4\n".to_vec();
        for i in 0..8u32 {
            let head = format!("{} 0 obj\n<< /Type /Page /N {} >>\n", i, i);
            pdf.extend_from_slice(head.as_bytes());
            let pad = 1024 * 1024 - head.len();
            pdf.extend(std::iter::repeat(b' ').take(pad));
        }
        let p = write_tmp("s5_noendobj", &pdf);
        let start = Instant::now();
        let _ = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_6-noendobj] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "缺 endobj 的扫描没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
    }

    #[test]
    fn s5_6_missing_endstream_search_stays_bounded() {
        // 2000 个「有 stream、没有 endstream」的对象（/Length 还是错的）：
        // 每个都会退化成向后搜索 endstream，旧代码每个都扫到文件尾 ——
        // 这个 3.7 MB 的攻击文件能扫出 GB 级的搜索量。
        let mut pdf: Vec<u8> = b"%PDF-1.4\n".to_vec();
        for i in 0..2000u32 {
            pdf.extend_from_slice(format!("{} 0 obj\n<< /Length 8 >>\nstream\n", i).as_bytes());
            pdf.extend(std::iter::repeat(b'x').take(1800));
            pdf.push(b'\n');
        }
        let p = write_tmp("s5_noendstream", &pdf);
        let start = Instant::now();
        let _ = extract_pdf_text(&p);
        let spent = start.elapsed();
        cleanup(&p);
        println!("[s5_6-noendstream] 耗时 {:?}", spent);
        assert!(
            spent < MALICIOUS_TIME_LIMIT,
            "缺 endstream 的向后搜索没有在 {:?} 内返回（实际 {:?}）",
            MALICIOUS_TIME_LIMIT,
            spent
        );
    }

    // ── B26：PNG 预测器 ──

    #[test]
    fn b26_png_predictor_object_stream_is_decoded() {
        // 页面树和字体全在带 PNG 预测器（/Predictor 12）的对象流里。
        // 旧代码在分支外面做 `data.len() % row_len != 0`，而 PNG 每行前面多一个 filter 字节，
        // 这个取模必然不等于 0 → 整条流被丢掉 → ObjStm 读不出来，中文变乱码/抽不到。
        let columns = 64usize;
        let (payload, first) = objstm_payload(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (3, page_obj(4, 5)),
            (4, cid_font(6)),
        ]);
        // 按 PNG 预测器编码：把负载补齐到 Columns 的整数倍，每行行首写一个 filter 字节 0（None）。
        let mut padded = payload;
        while padded.len() % columns != 0 {
            padded.push(b' ');
        }
        let mut encoded: Vec<u8> = Vec::with_capacity(padded.len() + padded.len() / columns);
        for row in padded.chunks(columns) {
            encoded.push(0);
            encoded.extend_from_slice(row);
        }
        let pdf = build_pdf_numbered(&[
            (5, stream_obj("", b"BT /F1 12 Tf 72 720 Td <00010002> Tj ET")),
            (6, stream_obj("", &cjk_bfchar_cmap())),
            (
                7,
                stream_obj(
                    &format!(
                        " /Type /ObjStm /N 4 /First {} /Filter /FlateDecode \
                         /DecodeParms << /Predictor 12 /Columns {} >>",
                        first, columns
                    ),
                    &zlib_bytes(&encoded),
                ),
            ),
        ]);
        // 明文里没有页面树字典：能抽到字就只能是从对象流里解出来的
        assert!(find_sub(&pdf, b"/Type /Catalog").is_none());
        let p = write_tmp("b26_png_predictor", &pdf);
        let got = extract_pdf_text(&p);
        cleanup(&p);
        assert_eq!(
            got.unwrap(),
            "你好",
            "带 PNG 预测器的对象流被丢掉了（B26）"
        );
    }

    // ── B27：混合码长的 CMap ──

    #[test]
    fn b27_mixed_one_and_two_byte_codespace() {
        // 1 字节和 2 字节混合的码空间：ASCII 走 <00><7F>（1 字节），汉字走 <8000><FFFF>（2 字节）。
        // 旧代码一律按最大码长 2 解码，<41 80 00> 会被切成 0x4180 / 0x00 两个不存在的码 → 乱码。
        let cmap = b"1 begincodespacerange\n<00> <7F>\nendcodespacerange\n\
1 begincodespacerange\n<8000> <FFFF>\nendcodespacerange\n\
2 beginbfchar\n<41> <0041>\n<8000> <4F60>\nendbfchar\n"
            .to_vec();
        let pdf = std_pdf(
            cid_font(6),
            b"BT /F1 12 Tf 72 720 Td <418000> Tj ET",
            false,
            vec![stream_obj("", &cmap)],
        );
        let p = write_tmp("b27_mixed", &pdf);
        let got = extract_pdf_text(&p).unwrap();
        cleanup(&p);
        assert_eq!(got, "A你", "混合码长解错了（B27）: {:?}", got);
    }
}
