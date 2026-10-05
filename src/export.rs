//! 批量导出：把会话写成 ChatLab Format v0.0.2 的 JSONL / JSON 落盘。
//!
//! # 这个模块不碰网络
//!
//! 取数由调用方**按页**回调给出（CLI 里是同工作区 SDK 的 `drain_session`）。这样切的理由：
//! 「怎么写盘」（行形状、slug 去重、幂等续跑、令牌不落盘）与「从哪儿取数」是两件不同的
//! 事——前者能在不起服务、不碰凭据的情况下被测全，后者的语义早被 SDK 自己的契约测试钉住。
//! 若在导出里自己拼 HTTP，同一个仓库就会有两份取数实现，而其中一份没有测试。
//!
//! # 两种格式的内存代价不同（这是刻意的取舍，不是没做完）
//!
//! - **jsonl**：页到即写、写完即丢，**内存与条数无关**。规范把 JSONL 定位成「超大规模
//!   记录（>100 万条）、流式、内存恒定」正是此意。代价是**不写 member 行**：流式写不出
//!   「先集齐成员再写消息」的顺序，而规范说成员行可选、缺省时由导入器从消息收集；本服务
//!   的消息行自带 `accountName` 与 `groupNickname`，所以信息不丢。
//! - **json**：一个会话一个完整信封，`members` 是规范的一等公民，因此整会话留在内存。
//!   「内存恒定」那条验收只在 jsonl 上跑，原因就在这里。
//!
//! # 两条硬约束
//!
//! 1. **导出物里不得出现访问令牌**。服务端的媒体以**根相对路径**给出（形如
//!    `/api/v1/media/<file>`，**不含令牌** —— 令牌只走请求头或 `?access_token=`，而响应体里
//!    从不嵌它）。导出物仍然刻意不写任何 URL：相对路径换台机器就失效，绝对路径更把导出物绑死
//!    在这一台。但光靠「我们没写」不够：将来任何人往消息里加一个链接字段，都可能顺手把凭据
//!    （或本地路径）落盘 —— 而导出文件会被拷进聊天工具、传上网盘、提交进仓库。所以
//!    `Options.secret` 由调用方给出（通常就是令牌本身），每一行写盘前做一次子串检查，命中即
//!    中止并删掉半成品。
//! 2. **`index.json` 是本仓自造的编排文件，不属于 ChatLab 规范**：导入器只吃单个
//!    `<slug>.jsonl`／`.json`，目录本身不可导入。这句话写在这里是为了下一个改这里的人
//!    不会以为「多写一份元数据没坏处」。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// 一个会话的导出目标：会话 id 与显示名。
#[derive(Debug, Clone)]
pub struct SessionTarget {
    pub talker: String,
    pub display_name: String,
}

/// 一条消息的中立形状：既不绑定生成层的 `PullMessage`，也不绑定服务端的 DTO，
/// 这样「怎么写」与「从哪取」可以各自被测。
#[derive(Debug, Clone)]
pub struct Row {
    pub platform_message_id: String,
    pub sender: String,
    pub account_name: String,
    pub group_nickname: String,
    pub timestamp: i64,
    pub msg_type: i64,
    pub content: String,
    pub reply_to_message_id: Option<String>,
    /// 媒体文件名（导出后的可取句柄）；无媒体时 `None`。
    pub media_file_name: Option<String>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// 规范的 JSONL 行式：逐行写、内存恒定。
    Jsonl,
    /// 一个会话一个完整信封：内存随条数增长（见模块头）。
    Json,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub out_dir: PathBuf,
    pub format: Format,
    /// 文件已在磁盘上就跳过该会话（幂等续跑）。
    pub resume: bool,
    /// 绝不允许出现在导出物里的串（通常是访问令牌）。
    pub secret: String,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub written: Vec<PathBuf>,
    pub skipped: Vec<String>,
    pub messages: u64,
    pub index: PathBuf,
}

/// 会话名 → 文件名（不含扩展名）。
///
/// 三条规则：
///
/// 1. 显示名可以是任意 Unicode（群名里有 emoji、有 `/`、有全角空格）——交给
///    `pathsafe::slugify`：它是全仓**唯一**的路径分量语义，已处理分隔符、冒号、尾点、
///    控制字符与 Windows 保留名；
/// 2. 显示名净化后为空时**回落到 talker**（同样过一遍 slugify，它含 `@` 与 `.`）；
/// 3. **碰撞处理**：slugify 截到 64 字符，两个长名字可能截成同一个，而同一个账号里也常有
///    同名会话——所以按 `taken` 追加 `-2`／`-3`…，并保证追加后仍在预算内。
///
/// 为什么文件名不直接用 talker：人拿到导出目录要能看懂（`项目群.jsonl` 比
/// `wxid_abc.jsonl` 有用），而 `--resume` 依赖「同一个会话每次落到同一个文件」，
/// 因此这里必须是**确定性**函数（同样的输入 + 同样的 taken 集合 → 同样的名字）。
pub fn file_name_for(target: &SessionTarget, taken: &mut BTreeSet<String>) -> String {
    // slugify 把非 ASCII 字母数字一律折成下划线（它是全仓唯一的路径分量语义，这里不自造
    // 第二份）。后果：纯中文群名会折成一串下划线——既不可读，又极易互相碰撞（"甲群"与
    // "乙群"折出来一模一样）。所以判据是「折叠后还剩不剩 ASCII 字母数字」：不剩就说明这次
    // 折叠把名字抹平了，改用 talker 的 slug —— 它是稳定唯一的会话 id，至少还能定位到会话。
    let display = crate::pathsafe::slugify(&target.display_name, "");
    let base = if display.chars().any(|c| c.is_ascii_alphanumeric()) {
        display
    } else {
        crate::pathsafe::slugify(&target.talker, "session")
    };
    // 留 8 个字符给 `-NNNNN` 后缀：追加之后也不能越出 slugify 自己的长度预算。
    let stem: String = base.chars().take(56).collect();
    let stem = if stem.is_empty() { "session".to_string() } else { stem };
    let mut name = stem.clone();
    let mut n = 2u32;
    while !taken.insert(name.clone()) {
        name = format!("{stem}-{n}");
        n += 1;
    }
    name
}

/// 规范里消息对象的形状（含 JSONL 的行型标记 `_type`）。
///
/// **刻意不写任何 URL**：服务端的媒体是根相对路径（`/api/v1/media/<file>`，令牌只走请求头），
/// 写进导出物只会让它在别的机器上失效。媒体在这里只以 `{type, fileName}` 出现，字节由
/// `--with-media` 现场下载到同目录 `media/` 下，文件名就是这里的 `fileName`。
fn message_line(r: &Row) -> Value {
    let mut v = json!({
        "_type": "message",
        "platformMessageId": r.platform_message_id,
        "sender": r.sender,
        "accountName": r.account_name,
        "timestamp": r.timestamp,
        "type": r.msg_type,
        "content": r.content,
    });
    if !r.group_nickname.is_empty() {
        v["groupNickname"] = json!(r.group_nickname);
    }
    // 与服务三个面同规：无引用时**省略该键**而不是给 null——规范把它列为可选 string，
    // null 会让按类型读取的导入器拿到解析不了的值。
    if let Some(rep) = &r.reply_to_message_id {
        v["replyToMessageId"] = json!(rep);
    }
    if let (Some(f), Some(t)) = (&r.media_file_name, &r.media_type) {
        v["media"] = json!({ "type": t, "fileName": f });
    }
    v
}

/// 把一行的媒体句柄**限定为确实落盘的那些**。
///
/// `--with-media` 给出的承诺是「导出物里出现的每一个 `fileName`，其字节都在同目录 `media/`
/// 下」。外链媒体与未能导出的媒体给不出这样的句柄，于是宁可**少一个 media 字段**，也不留下
/// 一个指向不存在文件的句柄 —— 后者会让导入器在几万条消息之后才发现缺件。
pub fn retain_downloaded_media(row: &mut Row, downloaded: &BTreeSet<String>) {
    if let Some(name) = row.media_file_name.as_ref()
        && !downloaded.contains(name)
    {
        row.media_file_name = None;
    }
}

/// 成员行（**只有 json 形态写**，理由见模块头）。
fn member_line(r: &Row) -> Value {
    let mut v = json!({
        "platformId": r.sender,
        "accountName": r.account_name,
    });
    if !r.group_nickname.is_empty() {
        v["groupNickname"] = json!(r.group_nickname);
    }
    v
}

/// 令牌泄漏的标记错误。
///
/// 为什么要专门一个类型：`run` 对「某个会话取数失败」的策略是**跳过并继续**（一次导出
/// 几百个会话，不该因为一个坏会话全盘作废），但令牌落盘**不能**降级成跳过——那等于悄悄
/// 把凭据写进一个会被拷走、上传、提交的文件。所以泄漏带这个标记，`run` 见到它立刻整轮
/// 中止，其余错误仍走跳过路径。
#[derive(Debug)]
pub struct SecretLeak;

impl std::fmt::Display for SecretLeak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "导出内容里出现访问令牌，写入已中止（媒体一律以 {{type, fileName}} 表达，不写 URL）")
    }
}

impl std::error::Error for SecretLeak {}

/// 检查一行是否带着不该出现的秘密。
///
/// 为什么不止比原文：写盘的是**序列化后**的文本，而秘密可能以转义或百分号编码的形态出现 ——
/// 只比原文会放过 `a\"b`（JSON 转义）与 `SEKRIT%2B123`（URL 编码）这类**可还原**的落盘形态。
/// 这里逐个比较同一份秘密的几种可还原表示，「都不命中」才算干净。
fn assert_no_secret(line: &str, secret: &str, file: &Path) -> Result<()> {
    if secret.is_empty() {
        return Ok(());
    }
    let json_escaped = serde_json::to_string(secret).unwrap_or_default();
    let json_escaped = json_escaped.trim_matches('"');
    let percent_encoded = percent_encode(secret);
    let leak = line.contains(secret)
        || (json_escaped != secret && line.contains(json_escaped))
        || (percent_encoded != secret && line.contains(&percent_encoded));
    if leak {
        return Err(anyhow::Error::new(SecretLeak)
            .context(format!("写入被中止: {}", file.display())));
    }
    Ok(())
}

/// 百分号编码（unreserved 集之外一律 `%XX`）。
///
/// 只用于「秘密是否以编码形态落盘」这一次比较，所以与具体实现策略无关地取**最保守**的那种：
/// 任何真实的编码器对同一串的编码结果都是它的子集或等价，而全编码能命中 `SEKRIT%2B123` 这类形态。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 一个会话的流式写手：`begin` → 若干次 `page` → `finish`。
///
/// 之所以是这个形状而不是「一次给全部行」：SDK 的 `drain_session` 本来就是**按页**回调，
/// 写手把页接过来直接落盘，行对象当场丢弃——这正是「内存与条数无关」的实现点。
/// `finish` 才把 json 形态的整信封序列化出去（那种形态必须把会话留在内存，见模块头）。
pub struct SessionWriter {
    path: PathBuf,
    format: Format,
    secret: String,
    /// jsonl：直接写文件句柄；json：把行攒在内存里。
    sink: Sink,
    count: u64,
    members: Vec<Value>,
    seen: BTreeSet<String>,
}

enum Sink {
    Lines(BufWriter<std::fs::File>),
    Buffer(Vec<Value>),
}

impl SessionWriter {
    pub fn begin(target: SessionTarget, opts: &Options, file_stem: String) -> Result<Self> {
        std::fs::create_dir_all(&opts.out_dir)
            .with_context(|| format!("创建导出目录失败: {}", opts.out_dir.display()))?;
        let ext = match opts.format {
            Format::Jsonl => "jsonl",
            Format::Json => "json",
        };
        let path = opts.out_dir.join(format!("{file_stem}.{ext}"));
        let chatlab = json!({
            "version": crate::server::chatlab::FORMAT_VERSION,
            "generator": crate::server::chatlab::GENERATOR,
            "exportedAt": chrono::Utc::now().timestamp(),
        });
        let meta = json!({
            "name": target.display_name,
            "platform": crate::server::chatlab::PLATFORM,
            "type": crate::server::chatlab::session_type(&target.talker),
            "groupId": target.talker,
        });
        let sink = match opts.format {
            Format::Jsonl => Sink::Lines(BufWriter::new(
                std::fs::File::create(&path)
                    .with_context(|| format!("创建导出文件失败: {}", path.display()))?,
            )),
            Format::Json => Sink::Buffer(Vec::new()),
        };
        let mut w = Self {
            path,
            format: opts.format,
            secret: opts.secret.clone(),
            sink,
            count: 0,
            members: Vec::new(),
            seen: BTreeSet::new(),
        };
        // 规范：第一行必须是 header（JSONL 的「Must be first line」）。
        //
        // **建了文件就要负责回收**：jsonl 的 File::create 在这一步之前已经落盘，若首行检查失败
        // （例如 display_name 里带着令牌），把空文件留在盘上等于给 --resume 挖一个「已完成」的坑。
        if let Err(e) = w.put(&json!({ "_type": "header", "chatlab": chatlab, "meta": meta })) {
            let _ = std::fs::remove_file(&w.path);
            return Err(e);
        }
        Ok(w)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }


    /// 收下一页。jsonl 立刻落盘并丢掉行；json 攒着。
    pub fn page(&mut self, rows: &[Row]) -> Result<()> {
        let mut sorted: Vec<&Row> = rows.iter().collect();
        // 规范建议按时间升序。这里只排**本页**：跨页排序会把内存恒定这条承诺打破，
        // 而取数侧（Pull 游标）本来就按时间排页，页内排序足够让产物单调可读。
        sorted.sort_by_key(|r| (r.timestamp, r.platform_message_id.clone()));
        for r in sorted {
            if self.format == Format::Json && self.seen.insert(r.sender.clone()) {
                self.members.push(member_line(r));
            }
            self.put(&message_line(r))?;
            self.count += 1;
        }
        Ok(())
    }

    fn put(&mut self, v: &Value) -> Result<()> {
        match &mut self.sink {
            Sink::Lines(f) => {
                let line = v.to_string();
                assert_no_secret(&line, &self.secret, &self.path)?;
                writeln!(f, "{line}").with_context(|| format!("写失败: {}", self.path.display()))?;
            }
            Sink::Buffer(b) => b.push(v.clone()),
        }
        Ok(())
    }

    /// 收尾。返回本会话写出的条数。**任何一步失败都会删掉半成品**：留下一个截断的
    /// `.jsonl` 比留下一个不存在的文件更糟——`--resume` 会把它当成已完成而永久跳过。
    pub fn finish(mut self) -> Result<(u64, PathBuf)> {
        let path = self.path.clone();
        let result = self.finish_inner();
        match result {
            Ok(n) => Ok((n, path)),
            // 失败即删：截断的 .jsonl 会被 --resume 当成已完成而永久跳过，
            // 一个不存在的文件比一个坏文件安全。
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    fn finish_inner(&mut self) -> Result<u64> {
        match &mut self.sink {
            Sink::Lines(f) => {
                // **不能吞错**：磁盘满或 I/O 错误会让缓冲尾部丢失，留下一个截断的 .jsonl，
                // 而本轮以「成功」收场（随后还会被 --resume 永久跳过）。上抛会走到 finish 的
                // 删半成品分支。
                f.flush()
                    .with_context(|| format!("flush 失败: {}", self.path.display()))?;
            }
            Sink::Buffer(b) => {
                // 头一行是 header，其余是消息；成员单独成组（规范：members 是数组字段）。
                let header = b.first().cloned().unwrap_or_else(|| json!({}));
                let strip_type = |v: &Value| {
                    let mut c = v.clone();
                    if let Some(o) = c.as_object_mut() {
                        // _type 是 JSONL 的行型标记；JSON 形态里消息是数组元素，
                        // 带上它等于给导入器塞一个规范没定义的键。
                        o.remove("_type");
                    }
                    c
                };
                let messages: Vec<Value> = b.iter().skip(1).map(strip_type).collect();
                let doc = json!({
                    "chatlab": header.get("chatlab").cloned().unwrap_or_else(|| json!({})),
                    "meta": header.get("meta").cloned().unwrap_or_else(|| json!({})),
                    "members": self.members,
                    "messages": messages,
                });
                let body = serde_json::to_string(&doc).context("序列化失败")?;
                assert_no_secret(&body, &self.secret, &self.path)?;
                std::fs::write(&self.path, body)
                    .with_context(|| format!("写失败: {}", self.path.display()))?;
            }
        }
        Ok(self.count)
    }
}

/// 写出 `index.json`（自造编排文件，见模块头第 2 条）。
pub fn write_index(
    opts: &Options,
    rows: &[Value],
) -> Result<PathBuf> {
    let index = opts.out_dir.join("index.json");
    let body = json!({
        "generated": {
            "tool": crate::server::chatlab::GENERATOR,
            "version": env!("CARGO_PKG_VERSION"),
            "at": chrono::Utc::now().timestamp(),
        },
        "note": "本文件是本服务自造的编排清单，**不是 ChatLab 规范的一部分**；导入请用单个 <name>.jsonl/.json，整个目录不可导入。",
        "sessions": rows,
    })
    .to_string();
    assert_no_secret(&body, &opts.secret, &index)?;
    std::fs::write(&index, body).with_context(|| format!("写索引失败: {}", index.display()))?;
    Ok(index)
}

/// 读上一轮的 `index.json`：`talker → 条目`。
///
/// **为什么需要它**：文件名里的编号是**按本次输入列表的顺序**算的 —— 列表一变（增删会话、某会话
/// 改名），同一个会话就会被算成另一个名字，于是在盘上留下第二份产物；而 `--resume` 跳过的会话若不进
/// 新清单，上一轮的 `index.json` 会被这一轮整个覆盖掉。两者都靠「先读上一轮」解决。
fn previous_index(out_dir: &Path) -> BTreeMap<String, Value> {
    let path = out_dir.join("index.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        // 清单坏了就当作没有：它不是规范的一部分，坏掉不该让整轮导出失败。
        return BTreeMap::new();
    };
    v.get("sessions")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    r.get("talker")
                        .and_then(Value::as_str)
                        .map(|t| (t.to_string(), r.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 既有产物是否**完整到可以跳过**。
///
/// 判据刻意便宜，但能抓住真实半成品：空文件（建了文件却失败、或进程在写首行前被杀），以及 jsonl
/// 末尾没有换行（flush 丢失、进程被杀留下的半行）。json 形态只查非空 —— 它的完整性要整份解析才
/// 知道，而「为一次跳过读整个文件」与本函数存在的理由相悖。
fn file_is_complete(path: &Path, format: Format) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    match std::fs::metadata(path) {
        Ok(m) if m.len() == 0 => false,
        Ok(_) => match format {
            Format::Json => true,
            Format::Jsonl => {
                let Ok(mut f) = std::fs::File::open(path) else {
                    return false;
                };
                if f.seek(SeekFrom::End(-1)).is_err() {
                    return false;
                }
                let mut last = [0u8; 1];
                f.read_exact(&mut last).is_ok() && last[0] == b'\n'
            }
        },
        Err(_) => false,
    }
}
/// 跑一次导出。
///
/// `fetch` 由调用方给：它负责「把这一会话的页喂进 `on_page`」。签名是回调而不是
/// `Vec`，因为「内存与条数无关」这条承诺要求中间不留行——只要这里返回 Vec，写手就
/// 必然把整会话攒在内存里，而那条验收在小语料上根本看不出差别。
///
/// 会话级失败不中断整轮（记进 `skipped` 继续下一个），但调用方**必须**因为
/// `skipped` 非空而以非零退出码说话：静默少导出几个会话是这类工具最坏的失败方式。
///
/// **调用方契约（本模块无法自己强制）**：若这次导出会留下媒体句柄（例如 CLI 的 `--with-media`），
/// 调用方必须在把每一行交给 `on_page` 之前调用 `retain_downloaded_media`，并把「确实落盘的名字集合」
/// 传进去。本模块不持有那个集合（下载发生在调用方），所以它写不出「有句柄必有字节」这条不变量；
/// 漏调的结果是**外链媒体也会原样落成 `media.fileName`** —— 导入器随后找不到那个文件。
pub fn run<F>(targets: &[SessionTarget], opts: &Options, mut fetch: F) -> Result<Outcome>
where
    F: FnMut(&SessionTarget, &mut dyn FnMut(&[Row]) -> Result<()>) -> Result<()>,
{
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("创建导出目录失败: {}", opts.out_dir.display()))?;
    let ext = match opts.format {
        Format::Jsonl => "jsonl",
        Format::Json => "json",
    };
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut index_rows: Vec<Value> = Vec::new();
    // `--resume` 要真是「续跑」：文件名与清单都必须以上一轮为准（见 previous_index 的说明）。
    let previous = if opts.resume { previous_index(&opts.out_dir) } else { BTreeMap::new() };

    for target in targets {
        let stem = match previous
            .get(&target.talker)
            .and_then(|r| r.get("file"))
            .and_then(Value::as_str)
        {
            Some(file) => {
                // 沿用上一轮的名字（去掉扩展名，本轮用当前格式的扩展名）。
                let stem = file
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(file)
                    .to_string();
                taken.insert(stem.clone());
                stem
            }
            None => file_name_for(target, &mut taken),
        };
        // 文件名先算出来才能判断 --resume：确定性（同样的输入→同样的名字）是续跑的前提。
        let path = opts.out_dir.join(format!("{stem}.{ext}"));
        if opts.resume && path.exists() {
            if !file_is_complete(&path, opts.format) {
                // 半成品**不能**当成已完成：否则 --resume 会永久跳过它。
                tracing::warn!("{} 的既有产物不完整（空文件或被截断），本轮重写它", target.talker);
            } else {
                skipped.push(target.talker.clone());
                // 跳过的会话也要进新清单，否则这次写出的 index.json 会把上一轮的条目抹掉。
                match previous.get(&target.talker) {
                    Some(row) => index_rows.push(row.clone()),
                    None => tracing::warn!(
                        "{} 有既有产物但上一轮清单里没有它，本次 index 不收录（无法得知条数）",
                        target.talker
                    ),
                }
                continue;
            }
        }
        let mut w = SessionWriter::begin(target.clone(), opts, stem)?;
        let failed = fetch(target, &mut |rows| w.page(rows)).err();
        if let Some(e) = failed {
            let p = w.path().to_path_buf();
            drop(w);
            let _ = std::fs::remove_file(&p);
            // 令牌泄漏不降级成「跳过这一个会话」：整轮中止，否则凭据已经写出去了。
            if e.downcast_ref::<SecretLeak>().is_some() {
                return Err(e);
            }
            tracing::warn!("跳过会话 {}：{e:#}", target.talker);
            skipped.push(target.talker.clone());
            continue;
        }
        let (n, path) = w.finish()?;
        // （finish 内部已经删掉半成品；能走到这里说明没有泄漏，因为泄漏是 Err。）
        total += n;
        let file = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        written.push(path);
        index_rows.push(json!({
            "talker": target.talker,
            "file": file,
            "messages": n,
            "displayName": target.display_name,
        }));
    }

    let index = write_index(opts, &index_rows)?;
    Ok(Outcome {
        written,
        skipped,
        messages: total,
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, ts: i64, sender: &str, nick: &str) -> Row {
        Row {
            platform_message_id: id.into(),
            sender: sender.into(),
            account_name: format!("{sender}-profile"),
            group_nickname: nick.into(),
            timestamp: ts,
            msg_type: 0,
            content: format!("body {id}"),
            reply_to_message_id: None,
            media_file_name: None,
            media_type: None,
        }
    }

    fn opts(dir: &Path, format: Format, resume: bool, secret: &str) -> Options {
        Options { out_dir: dir.to_path_buf(), format, resume, secret: secret.into() }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("exp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn files_with(dir: &Path, ext: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|p| p.file_name().to_string_lossy().into_owned())
            .filter(|s| s.ends_with(ext))
            .collect();
        v.sort();
        v
    }

    /// 读 JSONL 并逐行解析：规范说「每行必须是合法 JSON（不能跨行）」，所以解析失败
    /// 就是测试失败——顺带钉住「正文里的换行不会折断一行」这件事。
    fn read_jsonl(path: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).unwrap();
        text.lines()
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("该行不是合法 JSON: {e} / {l}"))
            })
            .collect()
    }

    #[test]
    fn jsonl_first_line_is_header_and_lines_carry_type_marks() {
        let dir = tmp("header");
        let targets = vec![SessionTarget { talker: "wxid_a".into(), display_name: "Group A".into() }];
        let o = opts(&dir, Format::Jsonl, false, "");
        run(&targets, &o, |_t, on_page| {
            on_page(&[row("1", 100, "u1", ""), row("2", 90, "u2", "card")])
        })
        .unwrap();
        let name = files_with(&dir, ".jsonl").remove(0);
        let lines = read_jsonl(&dir.join(&name));
        // 规范：第一行必须是 header。
        assert_eq!(lines[0]["_type"], "header", "首行类型: {}", lines[0]);
        assert_eq!(lines[1]["_type"], "message");
        assert_eq!(lines[2]["_type"], "message");
        // jsonl 刻意不写 member 行（流式写不出「先集齐成员」的顺序，理由见模块头）。
        assert!(!lines.iter().any(|v| v["_type"] == "member"), "jsonl 不该出现成员行");
        // 规范建议按时间升序：喂进去 100→90，写出来必须 90→100。
        assert_eq!(lines[1]["timestamp"], 90);
        assert_eq!(lines[2]["timestamp"], 100);
        // 群名片缺失时**省略该键**，不给空串也不给 null；有名片时原样带上。
        // 注意行序：页内按时间升序，喂进去 100(u1 无名片)→90(u2 有名片)，
        // 所以 lines[1] 是 90 那条（有群名片）。
        assert_eq!(lines[1]["groupNickname"], "card");
        assert!(lines[2].get("groupNickname").is_none(), "空群名片不该出现: {}", lines[2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_in_output_aborts_and_removes_partial_file() {
        let dir = tmp("secret");
        let targets = vec![SessionTarget { talker: "wxid_a".into(), display_name: "A".into() }];
        let o = opts(&dir, Format::Jsonl, false, "SEKRIT123");
        let err = run(&targets, &o, |_t, on_page| {
            let mut r = row("1", 100, "u1", "");
            r.content = "see ?access_token=SEKRIT123".into();
            on_page(&[r])
        })
        .expect_err("令牌出现在导出物里必须失败");
        assert!(format!("{err:#}").contains("访问令牌"), "报错要点明令牌问题: {err:#}");
        // 半成品必须被删掉：留着它，--resume 会把这个坏文件当成已完成而永久跳过。
        assert!(files_with(&dir, ".jsonl").is_empty(), "失败后仍留下产物");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_media_keeps_only_handles_whose_bytes_are_on_disk() {
        let mut have: BTreeSet<String> = BTreeSet::new();
        have.insert("deadbeef.png".to_string());

        let mut kept = row("1", 1, "u1", "");
        kept.media_file_name = Some("deadbeef.png".into());
        kept.media_type = Some("image".into());
        retain_downloaded_media(&mut kept, &have);
        assert_eq!(kept.media_file_name.as_deref(), Some("deadbeef.png"));

        let mut dropped = row("2", 2, "u1", "");
        dropped.media_file_name = Some("https-external.png".into());
        dropped.media_type = Some("image".into());
        retain_downloaded_media(&mut dropped, &have);
        assert!(dropped.media_file_name.is_none(), "没有本地副本就不能留下句柄");
    }

    #[test]
    fn message_line_omits_media_without_a_handle() {
        let mut r = row("1", 1, "u1", "");
        r.media_type = Some("image".into());
        assert!(
            message_line(&r).get("media").is_none(),
            "只有 type 没有落盘句柄时不该写出 media"
        );
    }

    #[test]
    fn slug_collision_gets_deterministic_suffix() {
        // 两个显示名折叠成同一个分量（真实账号里同名群常见），必须落到两个不同文件。
        let targets = [
            SessionTarget { talker: "wxid_1".into(), display_name: "Team/1".into() },
            SessionTarget { talker: "wxid_2".into(), display_name: "Team:1".into() },
        ];
        let mut taken = BTreeSet::new();
        let a = file_name_for(&targets[0], &mut taken);
        let b = file_name_for(&targets[1], &mut taken);
        assert_eq!(a, "Team_1");
        assert_eq!(b, "Team_1-2");
        // 确定性：同样输入再跑一次得到同样名字（--resume 依赖这一点）。
        let mut again = BTreeSet::new();
        assert_eq!(file_name_for(&targets[0], &mut again), a);
        assert_eq!(file_name_for(&targets[1], &mut again), b);
    }

    #[test]
    fn non_ascii_display_name_falls_back_to_talker_slug() {
        // 纯中文群名经 slugify 会折成一串下划线（不可读且互相碰撞），此时必须落到 talker。
        let t = SessionTarget { talker: "wxid_abc".into(), display_name: "甲群".into() };
        let mut taken = BTreeSet::new();
        assert_eq!(file_name_for(&t, &mut taken), "wxid_abc");
    }

    #[test]
    fn resume_skips_existing_without_fetching() {
        let dir = tmp("resume");
        std::fs::create_dir_all(&dir).unwrap();
        let targets = vec![SessionTarget { talker: "wxid_a".into(), display_name: "A".into() }];
        let o = opts(&dir, Format::Jsonl, true, "");
        run(&targets, &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        let mut fetched = 0;
        let got = run(&targets, &o, |_t, on_page| {
            fetched += 1;
            on_page(&[row("2", 2, "u1", "")])
        })
        .unwrap();
        assert_eq!(fetched, 0, "--resume 仍去取数了");
        assert_eq!(got.skipped, vec!["wxid_a".to_string()]);
        assert_eq!(got.messages, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_format_writes_members_and_one_document() {
        let dir = tmp("json");
        let targets = vec![SessionTarget { talker: "g@chatroom".into(), display_name: "G".into() }];
        let o = opts(&dir, Format::Json, false, "");
        let got = run(&targets, &o, |_t, on_page| on_page(&[row("1", 5, "u1", "card")])).unwrap();
        let text = std::fs::read_to_string(&got.written[0]).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["meta"]["type"], "group", "群会话的类型应来自 talker 后缀");
        assert_eq!(v["members"][0]["platformId"], "u1");
        assert!(v["messages"][0].get("_type").is_none(), "JSON 形态不该带行型标记");
        let idx: Value = serde_json::from_str(&std::fs::read_to_string(&got.index).unwrap()).unwrap();
        assert_eq!(idx["sessions"][0]["messages"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_session_is_removed_and_recorded_as_skipped() {
        let dir = tmp("fail");
        let targets = vec![
            SessionTarget { talker: "wxid_bad".into(), display_name: "Bad".into() },
            SessionTarget { talker: "wxid_ok".into(), display_name: "Good".into() },
        ];
        let o = opts(&dir, Format::Jsonl, false, "");
        let got = run(&targets, &o, |t, on_page| {
            if t.talker == "wxid_bad" {
                on_page(&[row("1", 1, "u1", "")])?;
                anyhow::bail!("mid-way fetch failure");
            }
            on_page(&[row("9", 9, "u9", "")])
        })
        .unwrap();
        assert_eq!(got.skipped, vec!["wxid_bad".to_string()], "失败会话要出现在 skipped 里");
        let left = files_with(&dir, ".jsonl");
        assert_eq!(left, vec!["Good.jsonl".to_string()], "半途而废的产物必须被删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_check_covers_escaped_and_percent_encoded_forms() {
        let dir = tmp("secret-forms");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.jsonl");
        // 原文命中
        assert!(assert_no_secret("prefix SEKRIT123 suffix", "SEKRIT123", &f).is_err());
        // JSON 转义形态：秘密含引号时盘上是 a\"b（只比原文会放过它）
        assert!(
            assert_no_secret("{\"c\":\"a\\\"b\"}", "a\"b", &f).is_err(),
            "转义形态必须命中"
        );
        // 百分号编码形态
        assert!(
            assert_no_secret("?access_token=SEKRIT%2B123", "SEKRIT+123", &f).is_err(),
            "编码形态必须命中"
        );
        // 干净的行不误报
        assert!(assert_no_secret("{\"c\":\"hello\"}", "a\"b", &f).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_failure_removes_the_created_file() {
        let dir = tmp("begin-cleanup");
        std::fs::create_dir_all(&dir).unwrap();
        let t = SessionTarget { talker: "wxid_leak".into(), display_name: "LEAKY-NAME".into() };
        let o = opts(&dir, Format::Jsonl, false, "LEAKY-NAME");
        assert!(SessionWriter::begin(t, &o, "probe".to_string()).is_err(), "首行带秘密时必须失败");
        let left = files_with(&dir, ".jsonl");
        assert!(left.is_empty(), "失败后不该留下空文件（--resume 会把它当已完成）: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_reuses_the_previous_file_name_and_keeps_the_index_row() {
        let dir = tmp("resume-stable");
        let a = SessionTarget { talker: "wxid_1".into(), display_name: "Team".into() };
        let b = SessionTarget { talker: "wxid_2".into(), display_name: "Team".into() };
        let o = opts(&dir, Format::Jsonl, false, "");
        run(&[a, b.clone()], &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        assert_eq!(files_with(&dir, ".jsonl").len(), 2, "同名会话应落到两个文件");
        // 第二轮只带 B：它必须沿用上一轮的 Team-2.jsonl，而不是重算成 Team.jsonl
        let o2 = opts(&dir, Format::Jsonl, true, "");
        let got = run(&[b], &o2, |_t, on_page| on_page(&[row("2", 2, "u2", "")])).unwrap();
        assert_eq!(got.skipped, vec!["wxid_2".to_string()], "B 应被识别为已完成");
        let idx: Value = serde_json::from_str(&std::fs::read_to_string(&got.index).unwrap()).unwrap();
        let rows = idx["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "跳过的会话也要留在清单里，否则上一轮的 index 被抹掉: {idx}");
        assert_eq!(rows[0]["file"], "Team-2.jsonl", "文件名必须沿用上一轮而不是重算漂移: {idx}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_rewrites_an_incomplete_artifact() {
        let dir = tmp("resume-incomplete");
        std::fs::create_dir_all(&dir).unwrap();
        // 造一个「存在但为空」的产物（建了文件却失败的现场）
        std::fs::write(dir.join("A.jsonl"), b"").unwrap();
        let o = opts(&dir, Format::Jsonl, true, "");
        let t = SessionTarget { talker: "wxid_a".into(), display_name: "A".into() };
        let got = run(&[t], &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        assert!(got.skipped.is_empty(), "空产物不该被当成已完成");
        assert_eq!(got.messages, 1);
        let body = std::fs::read_to_string(dir.join("A.jsonl")).unwrap();
        assert!(body.contains("\"_type\":\"message\""), "应当被重写: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
