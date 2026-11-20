//! 不透明进度游标 v3（两段式）。
//!
//! 一条游标必须能自证四件事，否则「拿别人的游标续拉」会静默串数据：它是**本进程**
//! 编的（内存索引的行标识没有跨进程意义）、属于**这个账号**、属于**这个会话**，
//! 且编它时的**注册代次**与**数据源世代**都还是当前这一代。
//!
//! 载荷（点分，编码前全是 ASCII 安全字符）：
//! `v3.<nonce>.<account8>.<registration>.<generation>.<session8>.<progress>`，
//! 整段再做 URL-safe base64（无填充）。**不能用标准字母表**：`+` 在 query 里会被
//! 解成空格，客户端原样回传时就变成另一条游标。
//!
//! 末段 `progress` 编码**两段式进度**：
//!
//! ```text
//! <phase>~p<create_time>_<sort_seq>_<local_id>~w<hex16>:<lid>[,<hex16>:<lid>…][~t<since>]
//! ```
//!
//! - `phase` 是 `1`（窗口快照段）或 `2`（入库补发段）；
//! - `p` 是第一段的**位置**：已交付集合里的最大时间键；
//! - `w` 是各分片的**行标识边界**：第一段是**冻结**的快照上界，进入第二段后退化为
//!   已补发的各分片水位；
//! - `t` 是窗口下界（初始请求的 `since`，无窗口时整段缺席）。
//!
//! **为什么要两段**。首页按时间键交付、续页若只按行标识过滤，两种序下的「已交付集合」
//! 互不为前缀：时间更晚、行标识更小的行会被 `行标识 > 水位` 直接跨过去，此后永不交付。
//! 把集合先按「链开始时的行标识上界」切成两段，每段内部只剩一种序，
//! 「已交付 = 该序的前缀」才成立：
//!
//! - 第一段：`in_window(m) && lid(m) ≤ snapshot(shard) && key(m) > position`，按时间键升序；
//! - 第二段：`lid(m) > snapshot(shard)`，按 `(分片, 行标识)` 升序，**不应用窗口**。
//!
//! 第一段**不推进** `snapshot`（推进会把「已被页切掉、但键更小」的新行跨过去）；
//! `snapshot` 取**已索引**上界而不是库文件上界——索引事后补齐的行行标识必然更大，
//! 因此落第二段、不会被两段同时排除。第二段不带窗口正是「排除线确立后入库的回溯行
//! 可达」这条既有承诺的载体，需要严格窗口的调用方（导出）在落盘前自行丢弃。
//!
//! 段切换**只在一处判**：第一段在 `end` 视作无上界时也为空才切。仅仅被 `end` 挡住
//! 不算耗尽——那些行的键大于 `position`，客户端抬高 `end` 后照样交付。
//!
//! 回归：`cursor_continuation_is_monotonic_when_row_order_diverges_from_time_order`
//! （两种序分叉时的续页不重放、不缺口）、`cursor_end_window_does_not_freeze_the_chain`
//! （被 `end` 挡住不进第二段）、契约用例 `cursor-window-drain-equals-single-page`
//! 与 `cursor-drain-equals-full-out-of-order`（翻到底的产物与单页等集）。
//!
//! 失效语义见 [`CursorError`]：语法坏 400；版本不认识、跨进程、跨账号、跨会话、
//! 代次不符一律 409 + `reason: cursor_reset_required`。**不静默退回时间模式**——
//! 那会让客户端以为续上了，实际上从头重放。

use crate::server::error::ApiError;

/// 游标总长上限（解码接受的最大字节数）。真实载荷不到 500 字节；余量留给
/// 分片极多的会话（每分片一段约 25–35 字节）。上限与编码端
/// （[`CursorProgress::encode`]）同源：编码按同一常数换算预算并**拒签**
/// 放不下的进度——上限与签发若不同源，多分片会话会拿到一条服务端自己都解不
/// 回的游标，续拉永远 400。
const MAX_LEN: usize = 2048;
const VERSION: &str = "v3";
/// 机器可读的失效原因：客户端据此**从头重拉**，而不是重试同一条游标。
pub const REASON_CURSOR_RESET: &str = "cursor_reset_required";

#[derive(Debug, PartialEq, Eq)]
pub enum CursorError {
    /// 语法坏：解不出来、字段数不对、数字或十六进制解析失败。这是**请求错误**（400）。
    Malformed,
    /// 进度放不进长度预算：这条游标**没有签出来**（500）。同会话的多分片排空
    /// 会线性增长进度段，超过预算时不能签出一条自己都解不回的游标。
    Overflow,
    /// 语法没问题，但这条游标对当前请求已经失效（409）；字符串是给人看的原因。
    Reset(&'static str),
}

impl CursorError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            CursorError::Malformed => {
                ApiError::bad_request("cursor 无法解析：期望服务端签发的原样游标")
            }
            CursorError::Overflow => ApiError::internal(
                "游标进度超出单条上限：会话分片过多，请先合并分片或缩短会话",
            ),
            CursorError::Reset(why) => ApiError::conflict_with_reason(REASON_CURSOR_RESET, why),
        }
    }
}

/// 编/解一条游标所需的「这一代」上下文。
#[derive(Clone, Copy)]
pub struct CursorContext {
    pub nonce: &'static str,
    pub account: [u8; 8],
    pub registration: u64,
    pub generation: u32,
    pub session: [u8; 8],
}

fn digest8(seed: &[u8]) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut out = [0u8; 8];
    out.copy_from_slice(&Sha256::digest(seed)[..8]);
    out
}

/// 分片代号的宽度（8 字节摘要的 16 位十六进制）。
const SHARD_HEX: usize = 16;

impl CursorContext {
    /// 按「当前进程 + 当前账号 + 当前会话 + 当前代次」构造上下文。
    pub fn for_session(account_wxid: &str, session: &str, registration: u64, generation: u32) -> Self {
        Self {
            nonce: crate::server::process_nonce(),
            account: digest8(account_wxid.as_bytes()),
            registration,
            generation,
            session: digest8(session.as_bytes()),
        }
    }

    /// 签发一条游标。进度放不进长度预算时返回 [`CursorError::Overflow`]——
    /// 服务端签出的每一条游标都必须能被自己的 [`Self::decode`] 解回。
    pub fn encode(&self, progress: &CursorProgress) -> Result<String, CursorError> {
        let progress = progress.encode(MAX_LEN).ok_or(CursorError::Overflow)?;
        let payload = format!(
            "{VERSION}.{}.{}.{}.{}.{}.{}",
            self.nonce,
            hex::encode(self.account),
            self.registration,
            self.generation,
            hex::encode(self.session),
            progress,
        );
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
    }

    pub fn decode(&self, cursor: &str) -> Result<CursorProgress, CursorError> {
        if cursor.is_empty() || cursor.len() > MAX_LEN {
            return Err(CursorError::Malformed);
        }
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| CursorError::Malformed)?;
        let text = String::from_utf8(raw).map_err(|_| CursorError::Malformed)?;
        let mut parts = text.split('.');
        let version = parts.next().ok_or(CursorError::Malformed)?;
        if version == "v1" || version == "v2" {
            // 旧形态（单水位 / 双键）与两段式不是同一套语义，续下去只会漏行，只能重拉。
            return Err(CursorError::Reset("这条游标来自更早的版本，无法继续使用"));
        }
        if version != VERSION {
            // 既不认识的版本也不是我们发过的旧版本：这是垃圾输入，不是「重拉」信号。
            return Err(CursorError::Malformed);
        }
        let nonce = parts.next().ok_or(CursorError::Malformed)?;
        let account = parts.next().ok_or(CursorError::Malformed)?;
        let registration = parts.next().ok_or(CursorError::Malformed)?;
        let generation = parts.next().ok_or(CursorError::Malformed)?;
        let session = parts.next().ok_or(CursorError::Malformed)?;
        let progress = parts.next().ok_or(CursorError::Malformed)?;
        if parts.next().is_some() {
            return Err(CursorError::Malformed);
        }
        if nonce != self.nonce {
            return Err(CursorError::Reset("这条游标由另一个服务进程签发，重启后必须从头拉取"));
        }
        // 身份字段**先验语法再比身份**：字段形态非法（非十六进制、长度不对）是
        // 语法坏（400），不是「身份对不上」（409）。两档处置相反——把语法错判成
        // 409 会让客户端对一条永远重试不成功的损坏游标执行全量重拉。
        let account = <[u8; 8]>::try_from(hex::decode(account).map_err(|_| CursorError::Malformed)?)
            .map_err(|_| CursorError::Malformed)?;
        if account != self.account {
            return Err(CursorError::Reset("这条游标属于另一个账号"));
        }
        let session = <[u8; 8]>::try_from(hex::decode(session).map_err(|_| CursorError::Malformed)?)
            .map_err(|_| CursorError::Malformed)?;
        if session != self.session {
            return Err(CursorError::Reset("这条游标属于另一个会话"));
        }
        let registration: u64 = registration.parse().map_err(|_| CursorError::Malformed)?;
        if registration != self.registration {
            return Err(CursorError::Reset("账号注册代次已变化（发生过注销或重新注册）"));
        }
        let generation: u32 = generation.parse().map_err(|_| CursorError::Malformed)?;
        if generation != self.generation {
            return Err(CursorError::Reset("数据源已更换（库文件身份变化）"));
        }
        CursorProgress::decode(progress).ok_or(CursorError::Malformed)
    }
}

/// 时间键：`(create_time, sort_seq, local_id)`。第一段按它升序扫描窗口。
///
/// 末分量 `local_id` 只在分片内唯一，因此键跨分片**不保证单射**。页边界「不切秒组」
/// 保证同 `create_time` 的行永不跨页——等键行必同页，`key > position` 的严格比较
/// 因此不会漏掉第二个。若哪天取消「不切秒组」，必须给键补上分片分量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeKey {
    pub create_time: i64,
    pub sort_seq: i64,
    pub local_id: i64,
}

/// 游标末段的进度：段号 + 第一段位置 + 各分片行标识边界 + 全局窗口下界。
///
/// 两个边界量在不同段里含义不同，但**字段只有一个**——`phase` 是权威开关，
/// 拆成两个字段会让「第一段误用推进函数」变成类型上允许的操作。第一段的推进走
/// [`CursorProgress::with_position`]，第二段走 [`CursorProgress::advance_phase2`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorProgress {
    /// `1` 窗口快照段 / `2` 入库补发段。
    phase: u8,
    /// 第一段：已交付集合里的最大时间键。第二段不再用它判交付。
    position: TimeKey,
    /// 各分片的行标识边界（按分片代号升序）：第一段是**冻结**的快照上界，
    /// 第二段是已补发的水位。线上没有的分片视为边界 0（该分片还没交付过任何行）。
    shards: Vec<(u64, i64)>,
    /// 窗口下界：初始请求的 `since`（无窗口 = `i64::MIN`）。**全局**的，不随分片
    /// 覆盖而取消——「该分片已有进度」不等于「这个分片的所有大行标识都在窗口内」。
    excluded_since: i64,
}

impl CursorProgress {
    /// 链开始时的第一段进度：快照取**已索引**的各分片行标识上界。
    ///
    /// 位置的时间分量取窗口下界而不是 0：第一段的窗口判据要求 `create_time >`
    /// 下界，因此窗口内的行时间分量必然更大，起点不会挡住任何本段该交付的行。
    /// 写 `(下界, 0, 0)` 而不是 `i64::MIN` 那一组，是为了载荷紧凑。
    pub fn start(snapshot: impl IntoIterator<Item = (u64, i64)>, since: Option<i64>) -> Self {
        let excluded_since = since.unwrap_or(i64::MIN);
        Self {
            phase: 1,
            position: TimeKey { create_time: excluded_since, sort_seq: 0, local_id: 0 },
            shards: sorted(snapshot),
            excluded_since,
        }
    }

    /// 空页结算（**没有**游标、窗口内没有可交付的行）：`end` 界内的一切都已确立。
    ///
    /// 位置的时间分量取 `watermark`（`end` 缺省时是当前时刻），`sort_seq` 取 `i64::MAX`
    /// 让同一秒的行**全部**落在位置之前：空页意味着这一秒也没有可交付的行，留一个
    /// 秒内序号空洞会让这些「已确立」的行在客户端丢掉时间过滤后重新冒出来。
    /// 时间更大的行键更大，仍然可达——这正是「被 `end` 挡住的行没有丢」的来源。
    pub fn settled(
        watermark: i64,
        max_lid: i64,
        snapshot: impl IntoIterator<Item = (u64, i64)>,
        since: Option<i64>,
    ) -> Self {
        Self {
            phase: 1,
            position: TimeKey { create_time: watermark, sort_seq: i64::MAX, local_id: max_lid },
            shards: sorted(snapshot),
            excluded_since: since.unwrap_or(i64::MIN),
        }
    }

    /// 空进度：从「什么都没有」起算。只用于自测与 [`Default`]——线上请求永远带着
    /// 一条由 `start` / `settled` / 解码得来的具体进度，签出空进度等于让客户端
    /// 从头重拉。
    pub fn empty() -> Self {
        Self {
            phase: 1,
            position: TimeKey::default(),
            shards: Vec::new(),
            excluded_since: i64::MIN,
        }
    }

    pub fn phase(&self) -> u8 {
        self.phase
    }

    pub fn position(&self) -> TimeKey {
        self.position
    }

    pub fn excluded_since(&self) -> i64 {
        self.excluded_since
    }

    /// 某分片当前的行标识边界（线上没有 = 0）。
    pub fn bound_of(&self, shard: u64) -> i64 {
        self.shards
            .iter()
            .find(|(s, _)| *s == shard)
            .map(|(_, b)| *b)
            .unwrap_or(0)
    }

    /// 第一段的判据：**快照内**且**键在位置之后**。窗口判据（`since` / `end`）由
    /// 调用方叠加——它们随请求变化，而游标只承载跨请求不变量。
    pub fn admits1(&self, shard: u64, key: TimeKey) -> bool {
        key > self.position && key.local_id <= self.bound_of(shard)
    }

    /// 第二段的判据：行标识在边界之后。**不看时间**——这一段就是「链开始后入库
    /// 的行无论时间戳都能经游标拿到」的载体。
    pub fn admits2(&self, shard: u64, local_id: i64) -> bool {
        local_id > self.bound_of(shard)
    }

    /// 第一段的位置推进：取本页最大键。第一段的行键都严格大于旧位置，因此不必
    /// 再与新值取较大者。**只允许在第一段调用**（第二段调它会造出「位置前进但
    /// 快照未动」的混合态）。
    pub fn with_position(&self, position: TimeKey) -> Self {
        debug_assert_eq!(self.phase, 1, "位置推进只属于第一段");
        Self { phase: 1, position, shards: self.shards.clone(), excluded_since: self.excluded_since }
    }

    /// 切到第二段：快照原样保留并开始充当水位（首次推进由 [`Self::advance_phase2`]
    /// 完成）。**单向**——第二段不再回到第一段，否则「位置停在旧值、快照已推进」
    /// 会让第一段的行永久够不到。
    pub fn into_phase2(&self) -> Self {
        Self {
            phase: 2,
            position: self.position,
            shards: self.shards.clone(),
            excluded_since: self.excluded_since,
        }
    }

    /// 第二段的水位推进：本页覆盖到的分片取「旧值与本页最大值」的较大者；没覆盖
    /// 到的分片保留原值。只前进不回退，因此已交付的行不会再次满足 [`Self::admits2`]。
    pub fn advance_phase2(&self, delivered: impl IntoIterator<Item = (u64, i64)>) -> Self {
        debug_assert_eq!(self.phase, 2, "水位推进只属于第二段");
        let mut shards = self.shards.clone();
        for (shard, lid) in delivered {
            match shards.binary_search_by_key(&shard, |(s, _)| *s) {
                Ok(i) => shards[i].1 = shards[i].1.max(lid),
                Err(i) => shards.insert(i, (shard, lid)),
            }
        }
        Self { phase: 2, position: self.position, shards, excluded_since: self.excluded_since }
    }

    /// 编码末段进度。放不进 `max_len` 换算出的预算时返回 `None`——调用方
    /// （[`CursorContext::encode`]）升格为拒签，服务端不签发解不回的游标。
    fn encode(&self, max_len: usize) -> Option<String> {
        // 容量模型（与 decode 的 MAX_LEN 同源）：先把总长上限（**编码后**长度）
        // 折回**原始载荷**预算，再扣身份字段——身份字段活在原始载荷里，base64 会把它
        // 一起膨胀 4/3，顺序反了就会少扣约三分之一的身份开销：多分片会话拿到一条
        // 超过上限、自己 decode 不回的游标，续拉永远 400。
        // 身份上限 `v3.<nonce16>.<account16>.<registration≤20>.<generation≤10>.<session16>`
        // 已含 6 个点分隔符；折算向下取整、扣减后再判，宁可少签不超签。
        // 回归：`issued_cursor_never_exceeds_the_length_ceiling`。
        let identity_len: usize = VERSION.len() + 6 + 16 + 16 + 20 + 10 + 16;
        let budget = max_len
            .checked_mul(3)?
            .checked_div(4)?
            .checked_sub(identity_len)?;
        let mut out = String::new();
        out.push(char::from(b'0' + self.phase));
        out.push_str(&format!(
            "~p{}_{}_{}~w",
            self.position.create_time, self.position.sort_seq, self.position.local_id
        ));
        for (i, (shard, bound)) in self.shards.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{shard:016x}:{bound}"));
        }
        if self.excluded_since != i64::MIN {
            out.push_str("~t");
            out.push_str(&self.excluded_since.to_string());
        }
        (out.len() <= budget).then_some(out)
    }

    /// 解析末段进度。返回 `None` = 语法坏（400）。
    fn decode(text: &str) -> Option<Self> {
        let mut segs = text.split('~');
        let phase = match segs.next()? {
            "1" => 1u8,
            "2" => 2u8,
            _ => return None,
        };
        let mut position: Option<TimeKey> = None;
        // 分片边界按代号收进 map：同一分片出现两次说明载荷不是我们签出的形态
        // （留着会让「取哪个边界」变成实现细节），判语法坏更诚实。
        let mut bounds: std::collections::BTreeMap<u64, i64> = std::collections::BTreeMap::new();
        let mut excluded_since = i64::MIN;
        let mut saw_watermarks = false;
        let mut saw_excluded = false;
        for seg in segs {
            if let Some(rest) = seg.strip_prefix('p') {
                if position.is_some() {
                    return None;
                }
                let mut nums = rest.split('_');
                let create_time = nums.next()?.parse().ok()?;
                let sort_seq = nums.next()?.parse().ok()?;
                let local_id = nums.next()?.parse().ok()?;
                if nums.next().is_some() {
                    return None;
                }
                position = Some(TimeKey { create_time, sort_seq, local_id });
            } else if let Some(rest) = seg.strip_prefix('w') {
                if saw_watermarks {
                    return None;
                }
                saw_watermarks = true;
                if rest.is_empty() {
                    continue;
                }
                for pair in rest.split(',') {
                    let (code, bound) = pair.split_once(':')?;
                    if code.len() != SHARD_HEX || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return None;
                    }
                    let shard = u64::from_str_radix(code, 16).ok()?;
                    let bound: i64 = bound.parse().ok()?;
                    if bounds.insert(shard, bound).is_some() {
                        return None;
                    }
                }
            } else {
                // 段标记只有 p / w / t 三个（首段已单独消费），其余一律语法坏——
                // 静默忽略等于让一条编错的游标继续生效。
                let rest = seg.strip_prefix('t')?;
                if rest.is_empty() || saw_excluded {
                    return None;
                }
                saw_excluded = true;
                excluded_since = rest.parse().ok()?;
            }
        }
        let position = position?;
        // `w` 段恒在（空边界表也编成 `~w`）：缺它就是不是我们签出的形态，
        // 静默当成「全部边界为 0」会让一条截断的游标从头重放整个会话。
        if !saw_watermarks {
            return None;
        }
        let shards: Vec<(u64, i64)> = bounds.into_iter().collect();
        Some(Self { phase, position, shards, excluded_since })
    }
}

impl Default for CursorProgress {
    fn default() -> Self {
        Self::empty()
    }
}

/// 排序 + 去重（同分片取最大边界）。签出的载荷里分片代号升序，解码也走同一套。
fn sorted(bounds: impl IntoIterator<Item = (u64, i64)>) -> Vec<(u64, i64)> {
    let mut out: Vec<(u64, i64)> = bounds.into_iter().collect();
    out.sort_unstable_by_key(|(s, _)| *s);
    out.dedup_by(|a, b| {
        if a.0 == b.0 {
            b.1 = b.1.max(a.1);
            true
        } else {
            false
        }
    });
    out
}

/// 分片代号的哈希（分片全名的 SHA-256 前 8 字节）。分片全名是「库文件相对路径:
/// 表名」，跨库同名表在代号上不碰撞（SHA-256 截断在会话内单分片数下不可能相撞）。
pub fn shard_code(shard_name: &str) -> u64 {
    let d = digest8(shard_name.as_bytes());
    u64::from_be_bytes(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CursorContext {
        CursorContext { nonce: "0123456789abcdef", account: [1; 8], registration: 3, generation: 7, session: [2; 8] }
    }

    #[test]
    fn round_trip_and_url_safety() {
        let progress = CursorProgress::start(
            [(shard_code("message/message_0.db:Msg_abc"), 42)],
            Some(20),
        );
        let encoded = ctx().encode(&progress).unwrap();
        assert_eq!(ctx().decode(&encoded), Ok(progress));
        // 标准字母表的 `+` / `/` 在 query 里会被改写，URL-safe 必须一个都不出现。
        assert!(!encoded.contains('+') && !encoded.contains('/'), "游标必须 URL-safe：{encoded}");
        assert!(!encoded.contains('='), "不留 padding，省一次转义：{encoded}");
    }

    #[test]
    fn empty_progress_round_trips_as_zero() {
        let encoded = ctx().encode(&CursorProgress::empty()).unwrap();
        assert_eq!(ctx().decode(&encoded), Ok(CursorProgress::empty()));
        // 空进度的线上形态是 `1~p0_0_0~w`：段号 + 零位置 + 空边界表。
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.ends_with(".1~p0_0_0~w"), "空进度的形态变了：{text}");
    }

    #[test]
    fn phase_two_round_trips_and_keeps_both_phases_distinct() {
        let a = shard_code("db0:Msg_a");
        let b = shard_code("db1:Msg_b");
        let phase1 = CursorProgress::start([(a, 100), (b, 10)], None);
        let phase2 = phase1
            .into_phase2()
            .advance_phase2([(a, 103), (b, 12)]);
        let encoded = ctx().encode(&phase2).unwrap();
        let decoded = ctx().decode(&encoded).unwrap();
        assert_eq!(decoded, phase2);
        assert_ne!(decoded, phase1, "段号必须进载荷");
        assert_eq!(decoded.phase(), 2);
        // 第一段的判据在第二段不再生效，反之亦然。
        assert!(decoded.admits2(b, 13));
        assert!(!decoded.admits2(b, 12), "已补发的水位本身不得重放");
    }

    #[test]
    fn phase_one_needs_both_the_snapshot_and_the_position() {
        let a = shard_code("db0:Msg_a");
        // 快照 100、位置 (50, 0, 0)。
        let progress = CursorProgress::start([(a, 100)], None).with_position(TimeKey {
            create_time: 50,
            sort_seq: 0,
            local_id: 0,
        });
        let key = |ts, seq, lid| TimeKey { create_time: ts, sort_seq: seq, local_id: lid };
        assert!(progress.admits1(a, key(60, 0, 7)), "快照内且键更大 ⇒ 交付");
        assert!(!progress.admits1(a, key(40, 0, 9)), "键不大于位置 ⇒ 已交付或已排除");
        assert!(!progress.admits1(a, key(40, 0, 0)));
        // 行标识超过快照 ⇒ 本条链不碰它（它属于第二段）。
        assert!(!progress.admits1(a, key(60, 0, 101)), "快照外的行必须留给第二段");
        assert!(progress.admits1(a, key(60, 0, 100)), "快照上界本身仍在第一段");
    }

    #[test]
    fn snapshot_is_frozen_while_phase_two_only_moves_the_watermark() {
        let a = shard_code("db0:Msg_a");
        let start = CursorProgress::start([(a, 100)], None);
        // 第一段推进位置：快照一个字节都不动——推进它会把「已被页切掉、但键更小」
        // 的新行跨过去（那正是单水位方案丢行的形态）。
        let moved = start.with_position(TimeKey { create_time: 90, sort_seq: 0, local_id: 50 });
        assert_eq!(moved.bound_of(a), 100, "第一段不得推进快照");
        assert_eq!(moved.position().create_time, 90);
        // 第二段推进水位：位置保持（它已不参与判交付）。
        let phase2 = start.into_phase2().advance_phase2([(a, 140)]);
        assert_eq!(phase2.bound_of(a), 140);
        assert_eq!(phase2.position(), start.position());
        assert!(!phase2.admits2(a, 140), "水位本身就是已补发的上界");
        assert!(phase2.admits2(a, 141));
    }

    #[test]
    fn phase_two_watermarks_never_regress_and_add_new_shards() {
        let a = shard_code("db0:Msg_a");
        let b = shard_code("db1:Msg_b");
        let c = shard_code("db2:Msg_c");
        let progress = CursorProgress::start([(a, 100), (b, 10)], None).into_phase2();
        // 本页只覆盖 a：b 原样保留。
        let next = progress.advance_phase2([(a, 150)]);
        assert_eq!(next.bound_of(a), 150);
        assert_eq!(next.bound_of(b), 10);
        // 覆盖到的新分片进入水位表。
        let next = next.advance_phase2([(c, 3)]);
        assert_eq!(next.bound_of(c), 3);
        // 覆盖值小于旧值时也不得回退。
        let next = next.advance_phase2([(a, 40)]);
        assert_eq!(next.bound_of(a), 150, "水位只前进不回退");
    }

    #[test]
    fn settled_position_swallows_the_whole_second_but_not_later_seconds() {
        let a = shard_code("db0:Msg_a");
        let settled = CursorProgress::settled(1_700_000_100, 7, [(a, 7)], Some(1_700_000_000));
        let key = |ts, seq, lid| TimeKey { create_time: ts, sort_seq: seq, local_id: lid };
        // 同一秒的行全部在位置之前：空页已经说明这一秒没有可交付的行。
        assert!(!settled.admits1(a, key(1_700_000_100, 0, 1)), "settled 的秒内序号不得留空洞");
        assert!(!settled.admits1(a, key(1_700_000_100, i64::MAX - 1, 7)));
        // 时间更大的行仍可达——被 `end` 挡住的行没有丢。
        assert!(settled.admits1(a, key(1_700_000_101, 0, 3)));
        // 窗口下界之下的行不可达（它们既不在窗口里，也没有「已交付」以外的含义）。
        assert!(!settled.admits1(a, key(1_699_999_999, 0, 3)));
    }

    #[test]
    fn shard_codes_are_stable_and_distinct_per_full_name() {
        let a = shard_code("message/message_0.db:Msg_aaaa");
        let b = shard_code("message/message_1.db:Msg_aaaa");
        assert_ne!(a, b, "不同库的同名表必须拿到不同代号");
        assert_eq!(a, shard_code("message/message_0.db:Msg_aaaa"));
    }

    #[test]
    fn e_prefixed_shard_codes_round_trip() {
        // 分片代号以十六进制 `e` 开头：段标记用 `p` / `w` / `t` 三个字母（都不与十六进制
        // 冲突），代号只出现在 `w` 段的 `:` 之前——两者形态正交，往返必须无损。
        let code = 0xe123_4567_89ab_cdefu64;
        let progress = CursorProgress::start([(code, 1), (0x0abc, 2)], Some(20))
            .into_phase2()
            .advance_phase2([(code, 3)]);
        let encoded = ctx().encode(&progress).unwrap();
        assert_eq!(ctx().decode(&encoded), Ok(progress));
        assert_eq!(ctx().decode(&encoded).unwrap().bound_of(code), 3);
    }

    #[test]
    fn eighteen_shards_with_epoch_bounds_round_trip_within_the_limit() {
        // 多分片 + 十位时间戳曾是「签出即解不回」的形态：编码预算与解码上限
        // 同源后，这类游标必须仍能完整往返。
        let bounds: Vec<(u64, i64)> = (1..=18).map(|c| (c, 1_700_000_000)).collect();
        let progress = CursorProgress::settled(1_700_000_000, 1_700_000_000, bounds, Some(1_699_000_000));
        let encoded = ctx().encode(&progress).unwrap();
        assert!(
            encoded.len() <= MAX_LEN,
            "签出的游标必须能被解码上限容纳：{} > {MAX_LEN}",
            encoded.len()
        );
        assert_eq!(ctx().decode(&encoded), Ok(progress));
    }

    #[test]
    fn progress_beyond_the_budget_is_refused_not_issued() {
        // 分片多到超出预算：拒签（Overflow）而不是签出一条解码必 400 的游标。
        let bounds: Vec<(u64, i64)> = (0..200).map(|c| (c, 1_700_000_000)).collect();
        assert_eq!(
            ctx().encode(&CursorProgress::start(bounds, None)),
            Err(CursorError::Overflow)
        );
    }

    #[test]
    fn issued_cursor_never_exceeds_the_length_ceiling() {
        // 身份字段按**上限**参与预算（nonce 16、注册代次 = u64::MAX、世代 = u32::MAX），
        // 分片数从小到大扫一遍：凡是签出来的游标，长度必须在上限内、且能解回；签不下的
        // 只能是拒签。预算公式若先扣身份再折算 4/3，就会在这一段里签出超过上限的游标
        // ——服务端自己解码必 400，续拉永远失败。
        let worst = CursorContext {
            nonce: "0123456789abcdef",
            account: [0xff; 8],
            registration: u64::MAX,
            generation: u32::MAX,
            session: [0xee; 8],
        };
        let mut issued = 0usize;
        let mut refused = 0usize;
        for n in 1..=200u64 {
            let mut bounds: Vec<(u64, i64)> = (0..n).map(|c| (c, 1)).collect();
            if let Some(last) = bounds.last_mut() {
                last.1 = i64::MAX;
            }
            match worst.encode(&CursorProgress::start(bounds, None)) {
                Ok(token) => {
                    issued += 1;
                    assert!(
                        token.len() <= MAX_LEN,
                        "签出的游标超过解码上限（{n} 个分片）：{} > {MAX_LEN}",
                        token.len()
                    );
                    assert!(
                        worst.decode(&token).is_ok(),
                        "签出的游标必须能被自己的 decode 解回（{n} 个分片）"
                    );
                }
                Err(CursorError::Overflow) => refused += 1,
                Err(e) => panic!("只允许成功或 Overflow，得到 {e:?}"),
            }
        }
        assert!(issued > 0 && refused > 0, "两个方向都要有样本：{issued} / {refused}");
    }

    #[test]
    fn malformed_progress_segments_are_malformed() {
        let bad = [
            "",                 // 空段
            "3~p1_0_0~w",        // 段号只有 1 / 2
            "1",                 // 缺位置段
            "1~p1_0~w",          // 位置少一个分量
            "1~p1_0_0_0~w",      // 位置多一个分量
            "1~p1_0_0",          // 缺边界段
            "1~p1_0_0~w~w",      // 边界段重复
            "1~p1_0_0~w0000000000000000", // 边界缺 `:`
            "1~p1_0_0~wzz:1",    // 代号不是十六进制
            "1~p1_0_0~w0000000000000000:1,0000000000000000:2", // 分片重复
            "1~p1_0_0~w~t",      // 排除线段为空
            "1~p1_0_0~w~x5",     // 未知段标记
            "1~p1_0_0~w~t5~t6",  // 排除线段重复（后者覆盖前者的静默错值）
        ];
        for text in bad {
            assert!(CursorProgress::decode(text).is_none(), "{text:?} 必须判语法坏");
        }
    }

    #[test]
    fn malformed_cursor_is_a_bad_request() {
        for bad in ["", "!!!", "AAAA", "djM", "djMu", "djMuYS5i"] {
            assert_eq!(ctx().decode(bad), Err(CursorError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn syntactically_broken_identity_fields_are_malformed_not_reset() {
        use base64::Engine as _;
        let encode = |payload: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        // nonce 正确、账号/会话字段形态非法：这是语法坏（400），不是身份不符（409）。
        let cases = [
            encode(format!("v3.{}.xyz.3.7.{}.1~p0_0_0~w", ctx().nonce, hex::encode([2u8; 8]))),
            encode(format!("v3.{}.{}.3.7.zz.1~p0_0_0~w", ctx().nonce, hex::encode([1u8; 8]))),
            encode(format!("v3.{}.{}.3.7.{}.1~p0_0_0~w", ctx().nonce, "aabb", "ccddeeff00112233")),
        ];
        for cursor in cases {
            assert_eq!(ctx().decode(&cursor), Err(CursorError::Malformed), "{cursor}");
        }
    }

    #[test]
    fn every_identity_mismatch_is_a_reset_not_a_silent_restart() {
        use base64::Engine as _;
        let encode = |payload: String| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        };
        let c = ctx();
        let cases = [
            (encode("v1.0.5".into()), "v1 旧版本"),
            (encode("v2.0.5".into()), "v2 旧版本"),
            (encode(format!("v3.{}.{}.{}.{}.{}.0", "ffffffffffffffff", hex::encode([1u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨进程"),
            (encode(format!("v3.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([9u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨账号"),
            (encode(format!("v3.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 3, 7, hex::encode([9u8; 8]))), "跨会话"),
            (encode(format!("v3.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 4, 7, hex::encode([2u8; 8]))), "注册代次"),
            (encode(format!("v3.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 3, 8, hex::encode([2u8; 8]))), "数据源世代"),
        ];
        for (cursor, what) in cases {
            assert!(
                matches!(c.decode(&cursor), Err(CursorError::Reset(_))),
                "{what} 必须判为需要重拉，而不是沿用或 400"
            );
        }
    }
}
