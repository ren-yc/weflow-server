//! 批量导出：把会话写成 ChatLab Format v0.0.2 的 JSONL / JSON 落盘。
//!
//! # 这个模块不碰网络
//!
//! 取数由调用方**按页**回调给出（CLI 里是同工作区 SDK 的 `pull_page` 逐页循环）。这样切的理由：
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
    /// 上一轮**完整交付**过的会话跳过（幂等续跑）。完成以 rename 为准：截断文件与 .part
    /// 残留一律重写。命中的会话进 `Outcome::reused`，不影响退出码。
    pub resume: bool,
    /// 绝不允许出现在导出物里的串（通常是访问令牌）。
    pub secret: String,
    /// 本轮交付包是否要自带媒体字节（CLI 的 `--with-media`）。本模块自己不下载，但
    /// 「句柄出现即可取」这条承诺**按轮次成立**，续跑判完成时需要知道这一轮在乎媒体。
    pub with_media: bool,
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub written: Vec<PathBuf>,
    /// 续跑命中既有完整产物的会话：这是幂等完成，不是失败——退出码只看 skipped。
    pub reused: Vec<String>,
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
    // 编排清单的名字先占掉：显示名恰好是 index 的会话若拿到 index.json，收尾时会被
    // write_index 整个覆盖成清单（json 形态下同名同扩展名），而清单的 file 还指向自己。
    taken.insert("index".to_string());
    let mut name = stem.clone();
    let mut n = 2u32;
    // 去重键按大小写折叠：Windows 卷默认大小写不敏感，Team.jsonl 与 team.jsonl 是同一个
    // 文件，只比精确串会让后写的会话覆盖先写的。集合只存折叠键，返回的名字保留原大小写。
    while !taken.insert(name.to_lowercase()) {
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

/// 把一行的媒体句柄改写成该消息**确实落盘**的那个名字（按消息 id 查映射）。
///
/// `--with-media` 给出的承诺是「导出物里出现的每一个 `fileName`，其字节都在同目录 `media/`
/// 下」。拉取面携带的 fileName 是索引里的原始名，消息面回填的是导出后的内容摘要名，两者
/// 不必相同——按名字对账会把下载成功的媒体引用整条删掉，所以映射的键是消息 id。映射里
/// 没有的消息（外链、未导出、名字被拒）宁可**少一个 media 字段**，也不留下指向不存在文件
/// 的句柄 —— 后者会让导入器在几万条消息之后才发现缺件。
pub fn retain_downloaded_media(row: &mut Row, downloaded: &BTreeMap<String, String>) {
    if row.media_file_name.is_some() {
        row.media_file_name = downloaded.get(&row.platform_message_id).cloned();
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
/// 这里先把整行做一次百分号解码，再逐个比较同一份秘密的几种可还原表示，「都不命中」才算
/// 干净。编码形的比较大小写不敏感：合法的百分号编码允许小写十六进制（%2b 与 %2B 同值），
/// 而本仓的编码器只产大写——只按大写比会漏掉小写的可还原形态；先解码则让任意编码器的
/// 产物（含部分编码）都还原回原文可比。
fn assert_no_secret(line: &str, secret: &str, file: &Path) -> Result<()> {
    if secret.is_empty() {
        return Ok(());
    }
    let json_escaped = serde_json::to_string(secret).unwrap_or_default();
    let json_escaped = json_escaped.trim_matches('"');
    let encoded_folded = percent_encode(secret).to_ascii_lowercase();
    let secret_folded = secret.to_ascii_lowercase();
    // 编码后与原文同值（秘密本身全是 unreserved 字节）就不再按折叠比：那会退化成大小写
    // 不敏感的原文匹配，把无关的普通词也一并拦下。
    let encoded_is_secret = encoded_folded == secret_folded;
    let decoded = percent_decode(line);
    for hay in [line, decoded.as_str()] {
        if hay.contains(secret) || (json_escaped != secret && hay.contains(json_escaped)) {
            return Err(anyhow::Error::new(SecretLeak)
                .context(format!("写入被中止: {}", file.display())));
        }
        if !encoded_is_secret && hay.to_ascii_lowercase().contains(&encoded_folded) {
            return Err(anyhow::Error::new(SecretLeak)
                .context(format!("写入被中止: {}", file.display())));
        }
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

/// 百分号解码：把 %XX 还原成字节，非法序列原样保留。
///
/// 只用于「秘密是否以编码形态落盘」这一次比较：先解码再比原文，任何编码器的产物
/// （大小写十六进制、部分编码）都还原回原文，比穷举编码形态更窄也更准。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 一个会话的流式写手：`begin` → 若干次 `page` → `finish`。
///
/// 之所以是这个形状而不是「一次给全部行」：SDK 的取数面本来就是**按页**回调，
/// 写手把页接过来直接落盘，行对象当场丢弃——这正是「内存与条数无关」的实现点。
/// `finish` 才把 json 形态的整信封序列化出去（那种形态必须把会话留在内存，见模块头）。
pub struct SessionWriter {
    path: PathBuf,
    /// 收尾前的落盘名（最终名加 .part 后缀）；finish 成功后改名成 path。
    part: PathBuf,
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
        // 先写 .part：rename 成最终名发生在 finish 全部成功之后——rename 就是「写完了」的
        // 记录，进程在页间被杀只会留下 .part，resume 对它一律重写。
        let path = opts.out_dir.join(format!("{file_stem}.{ext}"));
        let part = opts.out_dir.join(format!("{file_stem}.{ext}.part"));
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
                std::fs::File::create(&part)
                    .with_context(|| format!("创建导出文件失败: {}", part.display()))?,
            )),
            Format::Json => Sink::Buffer(Vec::new()),
        };
        let mut w = Self {
            path,
            part,
            format: opts.format,
            secret: opts.secret.clone(),
            sink,
            count: 0,
            members: Vec::new(),
            seen: BTreeSet::new(),
        };
        // 规范：第一行必须是 header（JSONL 的「Must be first line」）。
        //
        // **建了文件就要负责回收**：jsonl 的 File::create 在这一步之前已经落了 .part，首行检查
        // 失败就把它删掉——半成品不该过夜（resume 本来也不认 .part）。
        if let Err(e) = w.put(&json!({ "_type": "header", "chatlab": chatlab, "meta": meta })) {
            let _ = std::fs::remove_file(&w.part);
            return Err(e);
        }
        Ok(w)
    }

    /// 收尾前的落盘名（.part）：取数失败要清掉的是它，不是最终名。
    /// 最终名没有读取方 —— 它只由 `finish` 成功后经 rename 出现，并作为返回值交出。
    pub fn part_path(&self) -> &Path {
        &self.part
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

    /// 收尾。返回本会话写出的条数与最终路径。成功路径以 rename 收束：.part 改名成最终名，
    /// rename 就是「写完了」的记录。失败删掉 .part——最终名从头到尾不会出现，`--resume`
    /// 不会把半成品当成已完成。
    pub fn finish(mut self) -> Result<(u64, PathBuf)> {
        let path = self.path.clone();
        let part = self.part.clone();
        let result = self.finish_inner();
        match result {
            Ok(n) => match std::fs::rename(&part, &path) {
                Ok(()) => Ok((n, path)),
                Err(e) => {
                    // 改名本身会失败（目标被别的进程占着等）：一并删掉 .part，
                    // 别让一轮没有交付的东西留在盘上。
                    let _ = std::fs::remove_file(&part);
                    Err(anyhow::Error::new(e).context(format!(
                        "收尾改名失败: {} -> {}",
                        part.display(),
                        path.display()
                    )))
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                Err(e)
            }
        }
    }

    fn finish_inner(&mut self) -> Result<u64> {
        // 先把 sink 移出来：jsonl 的文件句柄必须在 rename 之前关闭（Windows 上带着打开的
        // 句柄改名并替换目标并不保证成功），json 形态则根本没有句柄可漏。
        let sink = std::mem::replace(&mut self.sink, Sink::Buffer(Vec::new()));
        match sink {
            Sink::Lines(mut f) => {
                // **不能吞错**：磁盘满或 I/O 错误会让缓冲尾部丢失，留下一个截断的 .jsonl，
                // 而本轮以「成功」收场（随后还会被 --resume 永久跳过）。上抛会走到 finish 的
                // 删半成品分支。
                f.flush()
                    .with_context(|| format!("flush 失败: {}", self.path.display()))?;
                drop(f);
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
                std::fs::write(&self.part, body)
                    .with_context(|| format!("写失败: {}", self.part.display()))?;
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
    // 清单同样先写 .part 再改名：它是续跑唯一的完成记录来源，一次半路被杀留下截断 JSON
    // 就会让 previous_index 当"没有上一轮"，把复用判据整条废掉。
    let index_part = opts.out_dir.join("index.json.part");
    std::fs::write(&index_part, body)
        .with_context(|| format!("写索引失败: {}", index_part.display()))?;
    std::fs::rename(&index_part, &index)
        .with_context(|| format!("改名索引失败: {} -> {}", index_part.display(), index.display()))?;
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
/// 判据刻意便宜，但能抓住真实残缺：空文件、jsonl 末尾没有换行（flush 丢失、进程被杀留下的
/// 半行）、json 缺收尾花括号。写盘已经改成先 .part 再 rename，最终名只在成功后出现——这些
/// 内容判据只是深度防御，防的是旧版本或外力留在最终名上的残缺文件。
fn file_is_complete(path: &Path, format: Format) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    match std::fs::metadata(path) {
        Ok(m) if m.len() == 0 => false,
        Ok(_) => match format {
            // json 以收尾花括号结尾：被截断的文档几乎总是缺它（完整信封以 } 收束）。
            Format::Json => {
                let Ok(mut f) = std::fs::File::open(path) else {
                    return false;
                };
                if f.seek(SeekFrom::End(-1)).is_err() {
                    return false;
                }
                let mut last = [0u8; 1];
                f.read_exact(&mut last).is_ok() && last[0] == b'}'
            }
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
/// 调用方必须在把每一行交给 `on_page` 之前调用 `retain_downloaded_media`，并把「消息 id →
/// 实际落盘名」的映射传进去，同时把 `Options::with_media` 设成本轮真实意图。本模块不持有那个映射
/// （下载发生在调用方），所以它写不出「有句柄必有字节」这条不变量：漏调映射的结果是**外链媒体也会
/// 原样落成 `media.fileName`**；漏设 `with_media` 的结果是**带媒体的续跑会复用上一轮的消息面
/// 产物**，而那一轮可能根本没下过媒体字节。两条都是导入器事后才会发现的缺件。
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
    let mut reused: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut index_rows: Vec<Value> = Vec::new();
    // 本轮**已经交付**的（会话 → 文件名）。!opts.resume && path.exists() 不能直接拒绝：
    // 全量重导同一个会话是合法用法（for t in targets 里同一个 talker 出现两次，第一次
    // 交付的文件就在盘上），只有**不属于本轮**的既有文件才是覆盖事故。
    let mut round_files: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    // `--resume` 要真是「续跑」：文件名与清单都必须以上一轮为准（见 previous_index 的说明）。
    let previous = if opts.resume { previous_index(&opts.out_dir) } else { BTreeMap::new() };
    // 非续跑轮也要**读**上一轮清单，只是不拿它做复用判据：盘上已有产物若不在本轮登记的
    // 名字集合里，taken 根本不认识它，于是编号会撞出一个同名文件并被 rename 静默覆盖
    // ——被覆盖的那个属于未在本轮的会话，而新一轮清单又没有它的条目，于是既看不见也
    // 不能自愈（子集轮最容易踩到：--session 少给一个会话，那个会话的产物就在射程内）。
    // 刻意**不**把清单登记的行播种进 taken：那会让撞名会话改拿 -2 留下第二份产物，
    // 而两份内容不同的同名产物对使用者比一次响亮拒绝更糟。回归位置：
    // subset_export_refuses_to_clobber_another_sessions_artifact。
    let recorded_index: BTreeMap<String, Value> =
        if opts.resume { previous.clone() } else { previous_index(&opts.out_dir) };
    // 上一轮的产物名先全部占位（按折叠键）：文件名编号按本轮输入列表的顺序算，新增会话
    // 排在旧会话前面时会抢走它的名字，旧会话随后沿用旧名就在大小写不敏感的卷上互相覆盖。
    for row in previous.values() {
        if let Some(file) = row.get("file").and_then(Value::as_str) {
            let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            taken.insert(stem.to_lowercase());
        }
    }

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
                if stem.eq_ignore_ascii_case("index") {
                    // 旧版清单留下的名字：index.json 永远属于编排文件，该会话改走确定性后缀
                    // 重新导出，否则清单与产物会互相覆盖。
                    file_name_for(target, &mut taken)
                } else {
                    taken.insert(stem.to_lowercase());
                    stem
                }
            }
            None => file_name_for(target, &mut taken),
        };
        // 文件名先算出来才能判断 --resume：确定性（同样的输入→同样的名字）是续跑的前提。
        let path = opts.out_dir.join(format!("{stem}.{ext}"));
        if !opts.resume && path.exists() {
            // 判据是**归属**，不是「本轮有没有写过」：既有产物属于别的会话、或谁都不
            // 认领时，本轮写下去就是把别人的交付物静默换成这个会话的内容——而新清单里
            // 没有那个会话的条目，于是它既看不见也不能自愈。同名重导（同一会话有意重跑）
            // 不在此列：那是「重导自己」，本轮的取数会覆盖自己的旧产物，语义正确。
            let name_here = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // 大小写折叠比对：去重集合与文件系统判定都是折叠口径（Windows 卷默认
            // 大小写不敏感），归属比对若用原始串，`Team.jsonl` 登记、本轮算出
            // `team.jsonl` 时会被判「无主」而误拒。
            let owner = recorded_index.iter().find_map(|(talker, row)| {
                row.get("file")
                    .and_then(Value::as_str)
                    .filter(|f| f.eq_ignore_ascii_case(&name_here))
                    .map(|_| talker.clone())
            });
            let own_rerun = owner.as_deref() == Some(target.talker.as_str());
            // 同一 talker 在本轮出现两次（targets 是调用方给的，不去重）：第二次是在
            // 覆盖本轮自己刚交付的产物，属同一语义，放行。
            let this_round = round_files
                .get(&target.talker)
                .map(|names| names.contains(&path))
                .unwrap_or(false);
            if !own_rerun && !this_round {
                let why = match &owner {
                    Some(o) => format!("它属于会话 {o}"),
                    None => "它没有被任何一轮清单认领（外部文件，或清单已丢失）".to_string(),
                };
                // 会话级失败，不是整轮中止：其余会话的交付不该被一次撞名连坐。
                // skipped 非空 ⇒ CLI 以 1 退出（既有口径），所以不新增退出码。
                // 起手前就拒，因此本轮没有 .part 需要清理。
                //
                // 被保护的属主必须继续留在清单里：把它那一行带进本轮清单。少了这步，
                // 本轮收尾的 write_index 只写本轮交付的条目，属主的产物就从清单上消失
                // —— 属主下次重导时，那个同名文件变成「无主」，被同一个拒绝机制挡住，
                // 唯一出路只剩手工删文件：拒绝机制自己造出死锁。
                if let Some(o) = &owner {
                    let missing = !index_rows.iter().any(|r| {
                        r.get("talker").and_then(Value::as_str) == Some(o.as_str())
                    });
                    let carried = missing.then(|| recorded_index.get(o)).flatten().cloned();
                    if let Some(row) = carried {
                        index_rows.push(row);
                    }
                }
                // 出路只列"删除该文件／换目录"这一条确定可行的：`--resume` 在这里不该
                // 被推荐——清单丢失时它同样认不出归属，会走"无完成记录 ⇒ 重写"的自愈
                // 分支把同名文件改写掉（边界钉在
                // resume_with_intact_index_avoids_the_collision_entirely：清单完好时
                // 续跑轮的名字已播种，根本撞不上，也就不需要这层拒绝）。
                tracing::warn!(
                    "跳过会话 {}：拒绝覆盖既有产物 {}（{why}）。出路：确认该文件确属本会话后删除它再重跑，或换一个输出目录",
                    target.talker,
                    path.display()
                );
                // 同一 talker 在本轮出现两次且两次都撞名：skipped 只记一次。
                if !skipped.contains(&target.talker) {
                    skipped.push(target.talker.clone());
                }
                continue;
            }
        }
        if opts.resume && path.exists() {
            // 复用必须同时满足三条，缺一条就重写：
            //   1. 上一轮的清单记着这个会话——「有个同名文件」不是完成记录。少了这条，
            //      从没导出过的会话只要算出同名就被静默判完成，而它在新一轮清单里又没有
            //      条目，于是既看不见也不能自愈（清单被删/被子集轮重写后特别容易踩到）。
            //   2. 没有 `.part` 残留——留着它说明有一轮起笔后没收住。
            //   3. 本轮承诺媒体时 `media/` 非空——「出现即可取」是按轮成立的：上一轮没带
            //      --with-media（或媒体目录被清理）就复用，导出物里每个 fileName 都会悬空。
            // 内容便宜判据（非空/末字节）仍然保留，防旧版本或外力留下的残缺文件。
            let name_here = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            // 不仅要看清单记没记这个会话，还要看它记的是不是**本轮这个文件名**：清单登记过
            // A.jsonl 而本轮 --format json 时，盘上的 A.json 若是上一轮换格式留下的孤儿，
            // 它再"完整"也不是本轮要的产物；切回 jsonl 时同理（旧孤儿会冒充新产物）。
            let recorded = previous
                .get(&target.talker)
                .is_some_and(|row| row.get("file").and_then(Value::as_str) == Some(name_here.as_str()));
            let part_left = opts.out_dir.join(format!("{stem}.{ext}.part")).exists();
            // 上一轮登记时带没带媒体，记在清单行里（`withMedia`）——数目录不行：一个根本没有
            // 媒体的语料永远不会建 `media/`，按目录判会让 `--with-media --resume` 每次全量重导，
            // 而它本来是无害的（导出物里没有句柄，承诺空真成立）。清单没有这个键的旧条目按"没带"处理。
            let media_kept = !opts.with_media
                || previous
                    .get(&target.talker)
                    .and_then(|r| r.get("withMedia"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            if recorded && !part_left && media_kept && file_is_complete(&path, opts.format) {
                // 命中既有完整产物是幂等完成，不是失败：退出码只看 skipped。
                reused.push(target.talker.clone());
                // 跳过的会话也要进新清单，否则这次写出的 index.json 会把上一轮的条目抹掉。
                if let Some(row) = previous.get(&target.talker) {
                    index_rows.push(row.clone());
                }
                round_files
                    .entry(target.talker.clone())
                    .or_default()
                    .insert(path.clone());
                continue;
            }
            // 半成品**不能**当成已完成：否则 --resume 会永久跳过它。
            tracing::warn!(
                "{} 的既有产物不可信（清单没有完成记录、残留 .part、缺媒体或内容不完整），本轮重写",
                target.talker
            );
        }
        // 起手失败（建目录或建 .part 失败）同样是**会话级**失败：跳过这一个、继续其余。
        // 整轮中止会把本轮已交付的会话留在盘上却不进清单——那比留一个 .part 更难收拾。
        // 令牌泄漏例外：凭据已经写出去了，必须立刻中止整轮。
        let mut w = match SessionWriter::begin(target.clone(), opts, stem) {
            Ok(w) => w,
            Err(e) => {
                if e.downcast_ref::<SecretLeak>().is_some() {
                    return Err(e);
                }
                tracing::warn!("跳过会话 {}（起手失败）：{e:#}", target.talker);
                skipped.push(target.talker.clone());
                continue;
            }
        };
        let failed = fetch(target, &mut |rows| w.page(rows)).err();
        if let Some(e) = failed {
            // 只清本轮的 .part：最终名从头到尾没被本轮碰过，上一轮的完整产物必须原样保留
            // （半途而废的是 .part，不是交付物本身）。
            let part = w.part_path().to_path_buf();
            drop(w);
            let _ = std::fs::remove_file(&part);
            // 令牌泄漏不降级成「跳过这一个会话」：整轮中止，否则凭据已经写出去了。
            if e.downcast_ref::<SecretLeak>().is_some() {
                return Err(e);
            }
            tracing::warn!("跳过会话 {}：{e:#}", target.talker);
            skipped.push(target.talker.clone());
            continue;
        }
        // 收尾失败（改名不回来等）与取数失败同级：跳过这一个会话。整轮中止会把本轮已经
        // 写出的会话留在盘上却不进清单，比留一个 .part 更难恢复。令牌泄漏例外——它整轮中止。
        let (n, path) = match w.finish() {
            Ok(v) => v,
            Err(e) => {
                if e.downcast_ref::<SecretLeak>().is_some() {
                    return Err(e);
                }
                tracing::warn!("跳过会话 {} 的收尾：{e:#}", target.talker);
                skipped.push(target.talker.clone());
                continue;
            }
        };
        // （finish 内部已经删掉半成品；能走到这里说明没有泄漏，因为泄漏是 Err。）
        total += n;
        let file = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        written.push(path.clone());
        round_files
            .entry(target.talker.clone())
            .or_default()
            .insert(path);
        index_rows.push(json!({
            "talker": target.talker,
            "file": file,
            "messages": n,
            "displayName": target.display_name,
            // 本轮交付包是否自带媒体字节：续跑时这是"能不能复用"的判据之一（见 run 的复用条件）。
            "withMedia": opts.with_media,
        }));
    }

    let index = write_index(opts, &index_rows)?;
    Ok(Outcome {
        written,
        reused,
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
        Options { out_dir: dir.to_path_buf(), format, resume, secret: secret.into(), with_media: false }
    }

    /// 带媒体意图的参数（`--with-media --resume` 的组合在测试里要能构造）。
    fn opts_media(dir: &Path, format: Format, resume: bool) -> Options {
        Options {
            out_dir: dir.to_path_buf(),
            format,
            resume,
            secret: String::new(),
            with_media: true,
        }
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
        assert!(
            files_with(&dir, ".part").is_empty(),
            "令牌熔断也不能留下 .part 中转文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_media_keeps_only_handles_whose_bytes_are_on_disk() {
        // 映射的键是消息 id：落盘名与拉取面给的原始名可以不同，按名字对账会把成功下载的
        // 媒体引用整条删掉（导出行仍引用原始名，磁盘上却是回填名）。
        let mut have: BTreeMap<String, String> = BTreeMap::new();
        have.insert("1".to_string(), "deadbeef.png".to_string());

        let mut kept = row("1", 1, "u1", "");
        kept.media_file_name = Some("original-name.png".into());
        kept.media_type = Some("image".into());
        retain_downloaded_media(&mut kept, &have);
        assert_eq!(
            kept.media_file_name.as_deref(),
            Some("deadbeef.png"),
            "句柄必须对齐到实际落盘的名字"
        );

        let mut dropped = row("2", 2, "u1", "");
        dropped.media_file_name = Some("https-external.png".into());
        dropped.media_type = Some("image".into());
        retain_downloaded_media(&mut dropped, &have);
        assert!(dropped.media_file_name.is_none(), "没有本地副本就不能留下句柄");

        // 本来没有媒体的行不该被别的 id 的落盘名凭空加上句柄。
        let mut no_media = row("1", 3, "u1", "");
        retain_downloaded_media(&mut no_media, &have);
        assert!(no_media.media_file_name.is_none(), "行没有媒体就不该长出句柄");
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

        // Windows 卷默认大小写不敏感：Team 与 team 会落到同一个文件，后写的覆盖先写的。
        // 去重判定必须折叠大小写，而交付名保留原大小写。
        let case_targets = [
            SessionTarget { talker: "wxid_t1".into(), display_name: "Team".into() },
            SessionTarget { talker: "wxid_t2".into(), display_name: "team".into() },
        ];
        let mut fold_taken = BTreeSet::new();
        let c1 = file_name_for(&case_targets[0], &mut fold_taken);
        let c2 = file_name_for(&case_targets[1], &mut fold_taken);
        assert_eq!(c1, "Team", "折叠判定不该改动交付名的大小写");
        assert_eq!(c2, "team-2", "大小写折叠后同名必须加后缀");
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
        // 命中既有完整产物是幂等完成（reused），不是失败（skipped）。
        assert_eq!(got.reused, vec!["wxid_a".to_string()]);
        assert!(got.skipped.is_empty(), "续跑命中不该进 skipped");
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

        // 显示名恰好叫 index 的会话：json 形态下它的产物与编排清单同名同扩展名，
        // 不预留就会「导出成功、文件却被清单覆盖」，而清单的 file 还指向自己。
        let dir2 = tmp("json-index-name");
        let targets2 = vec![SessionTarget { talker: "g@chatroom".into(), display_name: "index".into() }];
        let o2 = opts(&dir2, Format::Json, false, "");
        let got2 = run(&targets2, &o2, |_t, on_page| on_page(&[row("1", 5, "u1", "card")])).unwrap();
        assert_eq!(
            got2.written[0].file_name().unwrap().to_str().unwrap(),
            "index-2.json",
            "会话必须让出 index.json 给编排清单"
        );
        let env: Value = serde_json::from_str(&std::fs::read_to_string(&got2.written[0]).unwrap()).unwrap();
        assert_eq!(env["messages"].as_array().unwrap().len(), 1, "会话信封必须保持是信封");
        let idx2: Value = serde_json::from_str(&std::fs::read_to_string(&got2.index).unwrap()).unwrap();
        assert_eq!(idx2["sessions"][0]["file"], "index-2.json", "清单不得指向自己: {idx2}");
        let _ = std::fs::remove_dir_all(&dir2);
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
        // 小写十六进制（合法编码器允许大小写两种；本仓编码器只产大写，只比大写会放过它）
        assert!(
            assert_no_secret("?access_token=SEKRIT%2b123", "SEKRIT+123", &f).is_err(),
            "小写编码形态必须命中"
        );
        // 部分编码形态（只有 a 被编码）：先解码再比才抓得住
        assert!(
            assert_no_secret("token=%61bcdef0123456789", "abcdef0123456789", &f).is_err(),
            "部分编码形态必须命中"
        );
        // 干净的行不误报
        assert!(assert_no_secret("{\"c\":\"hello\"}", "a\"b", &f).is_ok());
        assert!(assert_no_secret("进度 50% 完成", "SEKRIT+123", &f).is_ok(), "普通百分号文本不该误报");
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
        assert_eq!(got.reused, vec!["wxid_2".to_string()], "B 应被识别为已完成");
        let idx: Value = serde_json::from_str(&std::fs::read_to_string(&got.index).unwrap()).unwrap();
        let rows = idx["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "跳过的会话也要留在清单里，否则上一轮的 index 被抹掉: {idx}");
        assert_eq!(rows[0]["file"], "Team-2.jsonl", "文件名必须沿用上一轮而不是重算漂移: {idx}");
        let _ = std::fs::remove_dir_all(&dir);

        // 第二轮换个目录重放「新增同名会话先到」：上一轮 A 叫 Team；本轮输入 [C, A]，
        // C 不在上一轮清单里。没有先播种时 C 会抢到 Team.jsonl 并被 A 的旧产物冒充成
        // 「已完成」，A 的数据从此导不出来。
        let dir2 = tmp("resume-newfirst");
        let a = SessionTarget { talker: "wxid_1".into(), display_name: "Team".into() };
        let o1 = opts(&dir2, Format::Jsonl, false, "");
        run(&[a], &o1, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        let c = SessionTarget { talker: "wxid_3".into(), display_name: "Team".into() };
        let a2 = SessionTarget { talker: "wxid_1".into(), display_name: "Team".into() };
        let mut fetched: Vec<String> = Vec::new();
        let o2 = opts(&dir2, Format::Jsonl, true, "");
        let got2 = run(&[c, a2], &o2, |t, on_page| {
            fetched.push(t.talker.clone());
            on_page(&[row("9", 9, "u9", "")])
        })
        .unwrap();
        assert_eq!(fetched, vec!["wxid_3".to_string()], "只有新会话需要取数，A 要复用旧产物: {fetched:?}");
        assert_eq!(got2.reused, vec!["wxid_1".to_string()]);
        assert_eq!(
            files_with(&dir2, ".jsonl"),
            vec!["Team-2.jsonl".to_string(), "Team.jsonl".to_string()],
            "新会话必须让开上一轮的文件名"
        );
        let idx2: Value = serde_json::from_str(&std::fs::read_to_string(&got2.index).unwrap()).unwrap();
        let files: Vec<String> = idx2["sessions"].as_array().unwrap().iter()
            .map(|r| r["file"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            files.iter().any(|f| f == "Team-2.jsonl") && files.iter().any(|f| f == "Team.jsonl"),
            "两条都要进清单: {idx2}"
        );
        let _ = std::fs::remove_dir_all(&dir2);
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

        // .part 半成品（新写法的崩溃现场）：哪怕内容看起来完整（合法 header + 一条
        // message + 行尾 LF），rename 没发生就是没写完——resume 必须重写，且 .part 不得残留。
        let dir2 = tmp("resume-part");
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(
            dir2.join("B.jsonl.part"),
            "{\"_type\":\"header\"}\n{\"_type\":\"message\",\"platformMessageId\":\"planted\"}\n",
        )
        .unwrap();
        let o2 = opts(&dir2, Format::Jsonl, true, "");
        let t2 = SessionTarget { talker: "wxid_b".into(), display_name: "B".into() };
        let mut fetched = 0;
        let got2 = run(&[t2], &o2, |_t, on_page| {
            fetched += 1;
            on_page(&[row("7", 7, "u7", "")])
        })
        .unwrap();
        assert_eq!(fetched, 1, ".part 一律重写（rename 前的名字天然不算完成）");
        assert_eq!(got2.messages, 1);
        assert!(!dir2.join("B.jsonl.part").exists(), "成功的收尾必须把 .part 改名掉");
        let body2 = std::fs::read_to_string(dir2.join("B.jsonl")).unwrap();
        assert!(body2.contains("\"platformMessageId\":\"7\""), "最终名应是本轮的完整产物: {body2}");
        assert!(!body2.contains("planted"), "旧的 .part 内容不得原样交出去: {body2}");
        let _ = std::fs::remove_dir_all(&dir2);

        // 截断的 json（旧版本或外力留在最终名上的残缺）：非空但缺收尾花括号，不得当成已完成。
        let dir3 = tmp("resume-json-cut");
        std::fs::create_dir_all(&dir3).unwrap();
        std::fs::write(dir3.join("C.json"), "{").unwrap();
        let o3 = opts(&dir3, Format::Json, true, "");
        let t3 = SessionTarget { talker: "wxid_c".into(), display_name: "C".into() };
        let got3 = run(&[t3], &o3, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        assert!(got3.reused.is_empty(), "截断的 json 不该被当成已完成");
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(dir3.join("C.json")).unwrap()).unwrap();
        assert_eq!(doc["messages"][0]["platformMessageId"], "1", "应当被重写成完整信封");
        let _ = std::fs::remove_dir_all(&dir3);
    }

    // 改名收尾的另一半：失败的那一轮不得把上一轮已经交付的完整产物一起带走。
    #[test]
    fn failed_rerun_keeps_the_previous_complete_artifact() {
        let dir = tmp("part-atomic");
        let t = SessionTarget { talker: "wxid_a".into(), display_name: "A".into() };
        let o = opts(&dir, Format::Jsonl, false, "");
        run(std::slice::from_ref(&t), &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        let first = std::fs::read_to_string(dir.join("A.jsonl")).unwrap();

        // 第二轮取数半途失败：新写法的失败只影响 .part，最终名保持原样。
        let got = run(&[t], &o, |_t, on_page| {
            on_page(&[row("2", 2, "u2", "")])?;
            anyhow::bail!("mid-way fetch failure")
        })
        .unwrap();
        assert_eq!(got.skipped, vec!["wxid_a".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("A.jsonl")).unwrap(),
            first,
            "失败轮不得破坏上一轮的完整产物"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

        fn mk(talker: &str, name: &str) -> SessionTarget {
        SessionTarget { talker: talker.into(), display_name: name.into() }
    }

    // 复核修复一：孤儿文件不能冒充完成记录。
    //
    // 复用判据不能只是"最终名存在且内容便宜判据通过"。清单被删或被截断时，本轮新算出的
    // 名字撞上别人的旧产物，就会把一个从没导出过的会话静默判成已完成——而它在新一轮清单
    // 里没有条目，于是既看不见也不能自愈。
    /// 子集导出撞名时**拒绝覆盖**别的会话的既有产物（会话级 skipped ⇒ 退 1）。
    ///
    /// 失败模式（修之前）：`taken` 从空开始、编号按本轮输入重算，于是本轮新会话能算出与
    /// 一个**未在本轮**的会话已交付产物同名的文件名，`.part` 收尾 rename 直接把它盖掉；
    /// 而新一轮 `index.json` 又没有那个会话的条目 ⇒ 交付物被换掉、清单不再提它、下一轮
    /// 也无从自愈。判据是**归属**（既有产物登记在另一个会话名下），不是「本轮写过没有」。
    #[test]
    fn subset_export_refuses_to_clobber_another_sessions_artifact() {
        let dir = tmp("clobber-refuse");
        // 第一轮：会话 a 交付成 "Team.jsonl"。
        run(
            &[mk("wxid_a", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("1", 1, "u1", "")]),
        )
        .unwrap();
        let first = std::fs::read_to_string(dir.join("Team.jsonl")).unwrap();
        assert!(first.contains("\"platformMessageId\":\"1\""), "第一轮产物基线: {first}");
        // 第二轮：只导会话 b，而它的显示名折叠后与 a 同名。
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_b", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("9", 9, "u9", "")])
            },
        )
        .unwrap();
        assert_eq!(got.skipped, vec!["wxid_b".to_string()], "撞名会话要进 skipped");
        assert_eq!(fetched, 0, "拒绝发生在起手前：不该去取数");
        assert!(got.written.is_empty(), "被拒的会话不该记成交付: {:?}", got.written);
        // 起手前就拒 ⇒ 连中转文件都不该留下（"不发请求"与"不留半成品"是两件事）。
        assert!(
            !dir.join("Team.jsonl.part").exists(),
            "拒绝不得留下 .part 半成品"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("Team.jsonl")).unwrap(),
            first,
            "会话 a 的既有产物必须原样保留，不能被会话 b 覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 但**同一会话有意重导**不算覆盖事故——它覆盖的是自己的旧产物。
    ///
    /// 若这里也拒，`export --session X` 重跑同一个会话就会永久失败（除非人先去删文件），
    /// 那是把「保护交付物」做成「交付物一旦生成就改不了」。判据用归属区分这两种情形：
    /// owner == 本会话 ⇒ 放行。回归位置：`intentional_rerun_of_same_session_overwrites_itself`。
    #[test]
    fn intentional_rerun_of_same_session_overwrites_itself() {
        let dir = tmp("rerun-self");
        let t = mk("wxid_a", "A");
        run(std::slice::from_ref(&t), &opts(&dir, Format::Jsonl, false, ""), |_t, on_page| {
            on_page(&[row("1", 1, "u1", "")])
        })
        .unwrap();
        let got = run(
            &[t],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("2", 2, "u2", "")]),
        )
        .unwrap();
        assert!(got.skipped.is_empty(), "重导自己的会话不该被拒: {:?}", got.skipped);
        let body = std::fs::read_to_string(dir.join("A.jsonl")).unwrap();
        assert!(body.contains("\"platformMessageId\":\"2\""), "第二轮内容应生效: {body}");
        assert!(!body.contains("\"platformMessageId\":\"1\""), "旧内容不该残留: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 被拒绝的**属主**下一轮必须仍能正常重导——拒绝机制不得自造死锁。
    ///
    /// 这是上一轮修出来的洞：被拒时本轮的 write_index 只写本轮交付的条目，
    /// 属主 Team.jsonl 就从清单上消失了；属主下次不带 --session 重导时，同名
    /// 文件算出的归属是「无主」⇒ 被同一个拒绝逻辑挡住，唯一出路只剩手工删文件。
    /// 修复是把属主那一行带进本轮清单；这条测试钉住它。
    /// 回归位置：`refusing_a_collision_keeps_the_owner_exportable`。
    #[test]
    fn refusing_a_collision_keeps_the_owner_exportable() {
        let dir = tmp("clobber-no-deadlock");
        run(
            &[mk("wxid_a", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("1", 1, "u1", "")]),
        )
        .unwrap();
        // 撞名轮：会话 b 被拒。
        run(
            &[mk("wxid_b", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("8", 8, "u8", "")]),
        )
        .unwrap();
        let idx: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).unwrap()).unwrap();
        assert!(
            idx["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["talker"] == "wxid_a" && r["file"] == "Team.jsonl"),
            "撞名轮必须把属主那一行留在清单里，否则下一轮它就是无主的: {idx}"
        );
        // 属主不带 --session 重导（非续跑轮）：自己的旧产物按归属放行。
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("3", 3, "u3", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "属主重导必须真的取数（未被拒）");
        assert!(got.skipped.is_empty(), "属主重导不该被拒: {:?}", got.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// `--resume` 的覆盖防护来自「上一轮清单的名字先播种进去重集合」这一条既有机制：
    /// 清单完好时，别的会话的产物名**算不出来**（会拿到 `-2` 后缀），所以根本撞不上。
    /// 这条测试钉住「拒绝覆盖」防护的真实边界：归属判定只在**清单可读**时成立；清单被删/截断时
    /// 「本轮无完成记录 ⇒ 重写」是既有且被需要的自愈语义（见
    /// `resume_needs_the_completion_record_not_just_a_matching_file`），此时无法区分
    /// 「自己的产物丢了记录」与「别人的产物」——归属校验方案早已被否决（需文件头），
    /// 因此告警文案不得把 `--resume` 说成无条件的安全出路。
    #[test]
    fn resume_with_intact_index_avoids_the_collision_entirely() {
        let dir = tmp("resume-taken-seed");
        run(
            &[mk("wxid_a", "Team")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("1", 1, "u1", "")]),
        )
        .unwrap();
        // 清单完好：续跑导会话 b，其显示名折叠后与 a 同名。
        let got = run(
            &[mk("wxid_b", "Team")],
            &opts(&dir, Format::Jsonl, true, ""),
            |_t, on_page| on_page(&[row("5", 5, "u5", "")]),
        )
        .unwrap();
        assert!(got.skipped.is_empty(), "续跑轮不该撞名被拒: {:?}", got.skipped);
        assert_eq!(
            got.written[0].file_name().unwrap().to_str().unwrap(),
            "Team-2.jsonl",
            "上一轮的名字已播种 ⇒ b 只能拿到后缀名"
        );
        assert!(
            std::fs::read_to_string(dir.join("Team.jsonl"))
                .unwrap()
                .contains("\"platformMessageId\":\"1\""),
            "会话 a 的产物必须没被动过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 既有产物**没有被任何一轮清单认领**（外力放的、或清单已丢失）时同样拒绝覆盖。
    ///
    /// 这是最安静的一类数据丢失：导出目录里躺着一个 `A.jsonl`，index 里却不提它，本轮
    /// 算出的名字正好撞上——若放行，那份「没人认领但确实存在」的文件就被换了内容，而
    /// 事后从清单里查不到它曾经存在过。
    #[test]
    fn unowned_leftover_file_is_not_clobbered() {
        let dir = tmp("clobber-unowned");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("A.jsonl"), "{ EXTERNAL, NOT OURS }\n").unwrap();
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("1", 1, "u1", "")])
            },
        )
        .unwrap();
        assert_eq!(got.skipped, vec!["wxid_a".to_string()], "无人认领的同名文件须拒绝覆盖");
        assert_eq!(fetched, 0, "拒绝发生在起手前");
        assert!(!dir.join("A.jsonl.part").exists(), "拒绝不得留下 .part 半成品");
        assert_eq!(
            std::fs::read_to_string(dir.join("A.jsonl")).unwrap(),
            "{ EXTERNAL, NOT OURS }\n",
            "外部文件内容必须原样保留"
        );
        // 先删掉它，重导就正常成功：拒绝只针对「不明来源的既有产物」。
        std::fs::remove_file(dir.join("A.jsonl")).unwrap();
        let got2 = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, false, ""),
            |_t, on_page| on_page(&[row("2", 2, "u2", "")]),
        )
        .unwrap();
        assert!(got2.skipped.is_empty(), "删除后重导应成功: {:?}", got2.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn resume_needs_the_completion_record_not_just_a_matching_file() {
        let dir = tmp("orphan-artifact");
        let o = opts(&dir, Format::Jsonl, false, "");
        run(&[mk("wxid_a", "A")], &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        let _ = std::fs::remove_file(dir.join("index.json"));
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, true, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("2", 2, "u2", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "没有完成记录的既有产物必须重导，而不是静默判完成");
        assert!(got.reused.is_empty(), "重导的会话不该记成复用: {:?}", got.reused);
        // 重导之后清单重新记着它，下一轮才允许复用
        let mut fetched2 = 0;
        let got2 = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, true, ""),
            |_t, on_page| {
                fetched2 += 1;
                on_page(&[row("3", 3, "u3", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched2, 0, "已有完成记录且产物完整时应当复用");
        assert_eq!(got2.reused, vec!["wxid_a".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 复核修复二：「出现即可取」是按轮成立的承诺。
    //
    // 上一轮不带 --with-media 时，导出行的 fileName 只是元数据、字节从未下载。本轮带媒体
    // 意图续跑若复用它，交付包里每个 fileName 都会悬空，而退出码仍然是 0。
    #[test]
    fn with_media_resume_redoes_sessions_whose_previous_round_had_no_media() {
        let dir = tmp("media-per-round");
        run(&[mk("wxid_a", "A")], &opts(&dir, Format::Jsonl, false, ""), |_t, on_page| {
            on_page(&[row("1", 1, "u1", "")])
        })
        .unwrap();
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "A")],
            &opts_media(&dir, Format::Jsonl, true),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("2", 2, "u1", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "上一轮没下媒体时，带媒体续跑不得复用");
        assert!(got.reused.is_empty(), "{:?}", got.reused);
        // 本轮登记了 withMedia，第三次才可以复用
        let mut fetched2 = 0;
        let got2 = run(
            &[mk("wxid_a", "A")],
            &opts_media(&dir, Format::Jsonl, true),
            |_t, on_page| {
                fetched2 += 1;
                on_page(&[row("3", 3, "u1", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched2, 0, "上一轮自带媒体且产物完整时应当复用");
        assert_eq!(got2.reused, vec!["wxid_a".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 复核修复三：换格式留下的孤儿不是本轮的产物。
    #[test]
    fn format_switch_does_not_reuse_the_other_formats_orphan() {
        let dir = tmp("format-switch");
        run(&[mk("wxid_a", "A")], &opts(&dir, Format::Jsonl, false, ""), |_t, on_page| {
            on_page(&[row("1", 1, "u1", "")])
        })
        .unwrap();
        // 清单登记的是 A.jsonl；本轮要 A.json。盘上那个 json 是外力/旧轮留下的孤儿。
        std::fs::write(dir.join("A.json"), "{\"a\":1}").unwrap();
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Json, true, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("2", 2, "u2", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "清单登记的文件名与本轮不同，孤儿不算本轮产物");
        assert!(got.reused.is_empty());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("A.json")).unwrap()).unwrap();
        assert_eq!(doc["messages"][0]["platformMessageId"], "2", "孤儿必须被重写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 复核修复四：残留的 .part 说明上一轮起笔后没收住。
    #[test]
    fn leftover_part_file_blocks_reuse() {
        let dir = tmp("part-blocks-reuse");
        run(&[mk("wxid_a", "A")], &opts(&dir, Format::Jsonl, false, ""), |_t, on_page| {
            on_page(&[row("1", 1, "u1", "")])
        })
        .unwrap();
        std::fs::write(dir.join("A.jsonl.part"), b"half-written").unwrap();
        let mut fetched = 0;
        let got = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, true, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("3", 3, "u3", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "残留 .part 时不得复用");
        assert!(got.reused.is_empty());
        assert!(!dir.join("A.jsonl.part").exists(), "重导的收尾必须把 .part 改名掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 复核修复五：单个会话收尾失败不得牵连整轮。
    //
    // 整轮中止会让本轮已经写出的会话留在盘上却不进清单——那比留一个 .part 更难收拾，
    // 也和"会话级失败只跳过"的既有口径自相矛盾。
    #[test]
    fn one_sessions_finish_failure_does_not_abort_the_round() {
        let dir = tmp("finish-skip");
        // 把最终名占成一个非空目录：收尾改名必然失败，模拟"目标被别的东西占着"
        let blocked = dir.join("B.jsonl");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("keep"), b"x").unwrap();
        let o = opts(&dir, Format::Jsonl, false, "");
        let got = run(&[mk("wxid_a", "A"), mk("wxid_b", "B")], &o, |_t, on_page| {
            on_page(&[row("1", 1, "u1", "")])
        })
        .unwrap();
        assert_eq!(got.skipped, vec!["wxid_b".to_string()], "收尾失败应只跳过该会话");
        assert_eq!(got.written.len(), 1, "另一个会话仍应交付");
        let idx: Value =
            serde_json::from_str(&std::fs::read_to_string(&got.index).unwrap()).unwrap();
        let rows = idx["sessions"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "本轮已交付的会话必须进清单: {idx}");
        assert_eq!(rows[0]["talker"], "wxid_a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 复核修复六：清单自己也要原子写。
    //
    // 它是续跑唯一的完成记录来源。一次半路被杀留下截断 JSON，previous_index 就当"没有
    // 上一轮"，于是复用判据整条被废——正是修复一/三/四要防的那条覆盖链。
    #[test]
    fn index_is_written_atomically_and_a_truncated_one_does_not_silently_reuse() {
        let dir = tmp("index-atomic");
        let o = opts(&dir, Format::Jsonl, false, "");
        let got = run(&[mk("wxid_a", "A")], &o, |_t, on_page| on_page(&[row("1", 1, "u1", "")])).unwrap();
        assert_eq!(got.written.len(), 1);
        assert!(!dir.join("index.json.part").exists(), "清单收尾应改名，不留中转文件");
        // 清单出现在最终名上就必须是一份可读的清单——原子性的全部意义
        let idx: Value =
            serde_json::from_str(&std::fs::read_to_string(&got.index).unwrap()).unwrap();
        assert_eq!(idx["sessions"].as_array().unwrap().len(), 1, "清单登记本轮交付: {idx}");

        // 把清单截断成半份 JSON：下一轮不得因为"产物完整"就复用
        std::fs::write(dir.join("index.json"), "{\"sessions\":[").unwrap();
        let mut fetched = 0;
        let got2 = run(
            &[mk("wxid_a", "A")],
            &opts(&dir, Format::Jsonl, true, ""),
            |_t, on_page| {
                fetched += 1;
                on_page(&[row("5", 5, "u5", "")])
            },
        )
        .unwrap();
        assert_eq!(fetched, 1, "清单坏了就当没有完成记录，必须重导");
        assert!(got2.reused.is_empty());
        let idx: Value =
            serde_json::from_str(&std::fs::read_to_string(&got2.index).unwrap()).unwrap();
        assert_eq!(idx["sessions"].as_array().unwrap().len(), 1, "重导后清单恢复: {idx}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
