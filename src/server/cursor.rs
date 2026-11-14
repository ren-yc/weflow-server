//! 不透明进度游标 v2。
//!
//! 一条游标必须能自证四件事，否则「拿别人的游标续拉」会静默串数据：它是**本进程**
//! 编的（内存索引的行标识没有跨进程意义）、属于**这个账号**、属于**这个会话**，
//! 且编它时的**注册代次**与**数据源世代**都还是当前这一代。
//!
//! 载荷（点分，编码前全是 ASCII 安全字符）：
//! `v2.<nonce>.<account8>.<registration>.<generation>.<session8>.<progress>`，
//! 整段再做 URL-safe base64（无填充）。**不能用标准字母表**：`+` 在 query 里会被
//! 解成空格，客户端原样回传时就变成另一条游标。
//!
//! 末段 `progress` 编码**各分片的行标识排除**：`local_id` 只在 `source_shard`
//! （库文件:表名）内唯一，而同一会话的行可以来自多个分片表（真实库里 16 与 32
//! 位 md5 表共存；不同文件也可能各有一张同名 md5 表）。只编码会话最大
//! `local_id` 会把低分片的新追加行永远滤在「`local_id > 上界`」之外——且普通
//! 追加不推进数据源世代，客户端拿不到任何重置信号。每分片两个量：**水位线**
//! （已交付最大行标识）与**历史高水位**（排除线界内历史最大行标识）；另有全局
//! 的**排除线**（初始 `since`）。排除线段 `t<since>` 与高水位后缀 `-<n>` 中的
//! `t` 取自十六进制字母表之外，分片代号（十六进制开头）不会与之混淆。段间
//! `~` 分隔，空进度（从头拉取）编 `0`。编码容量与 [`CursorContext::decode`]
//! 的总长上限**同源**（同一 [`MAX_LEN`] 换算预算）：放不下的进度拒绝签发，
//! 服务端签出的游标必然能解回来。游标不跨进程存活（nonce 绑定本进程），不存
//! 在需要兼容的旧形态。
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
const VERSION: &str = "v2";
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
        if version == "v1" {
            // v1 及更早的形态没有账号/会话/世代绑定，语义已经变了，只能重拉。
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

/// 游标末段的进度：会话内每个分片的**水位线**（已交付最大行标识）与**历史高
/// 水位**（排除线界内历史最大行标识），加上全局**排除线**（初始 `since`）。
///
/// 「定局」= 行要么已被交付，要么属于排除线界内的历史且此后也不会进入任何
/// 窗口。排除判据按行标识而不是时间：时间序与行标识序在回溯行上分叉（时间
/// 更早、入库更晚的行 local_id 更大），按时间排除的续拉会把同分片的历史回溯
/// 行放进来；行标识排除对它们成立。单标量下界不够——历史行可能插在未交付
/// 窗口行之后（lid 更大），把历史 lid 并进水位线会把那条未交付窗口行永久滤掉；
/// 两个量各管一类行：
///
/// - `local_id ≤ 水位线` ⇒ 已交付，不得重放；
/// - `create_time ≤ 排除线 且 local_id ≤ 历史高水位` ⇒ 签发时刻已在库的窗口外
///   历史，不得冒出；
/// - 其余行（含排除线确立之后入库的回溯行：local_id 按分片内 AUTOINCREMENT
///   单调，必然大于签发时刻的全部行标识）照常可达。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorProgress {
    /// `(分片代号, 水位线, 历史高水位)`，按代号升序。高水位 `None` = 排除线
    /// 界内该分片没有历史行（任何时间早于排除线的行都是其后入库的回溯行，
    /// 可交付）。游标线上没有的分片视为从头拉取——排除线确立时它不存在，
    /// 此后出现的新分片必然推进数据源世代、触发游标重置。
    shards: Vec<(u64, i64, Option<i64>)>,
    /// 全局排除线：初始请求的 since（不排除历史 = `i64::MIN`）。它是**全局**
    /// 的，不随分片被覆盖而取消——「分片已有进度」不等于「这个分片的所有大
    /// 行标识都在窗口内」（同分片的历史回溯行正是反例）。
    excluded_since: i64,
}

impl CursorProgress {
    /// 从会话里各分片的已交付上界构造（无排除线——纯增量链、无 since 请求，
    /// 或游标模式本页覆盖的分片）。调用方只传「本次响应真正覆盖到的分片」；
    /// 游标线上没有的分片进度由 [`CursorProgress::merged`] 保留。
    pub fn from_bounds(bounds: impl IntoIterator<Item = (u64, i64)>) -> Self {
        Self {
            shards: bounds.into_iter().map(|(s, b)| (s, b, None)).collect(),
            excluded_since: i64::MIN,
        }
        .normalized()
    }

    /// 带排除线的构造（初始请求带 since 的时间模式）：`delivered` 是各分片
    /// **已交付**的最大行标识（空页为空），`history` 是各分片**排除线界内**
    /// （`create_time ≤ since`）历史行的最大行标识。界内无历史行的分片高水位
    /// 为 `None`——其时间早于排除线的行都是其后入库的回溯行，照常可达。
    pub fn with_exclusion(
        delivered: impl IntoIterator<Item = (u64, i64)>,
        history: impl IntoIterator<Item = (u64, i64)>,
        since: i64,
    ) -> Self {
        let mut shards: Vec<(u64, i64, Option<i64>)> = delivered
            .into_iter()
            .map(|(s, b)| (s, b, None))
            .chain(history.into_iter().map(|(s, b)| (s, 0, Some(b))))
            .collect();
        shards.sort_unstable_by_key(|(s, _, _)| *s);
        Self { shards, excluded_since: since }.normalized()
    }

    /// 空进度：全部行从 0 起算（「从头拉取」的语义位）。
    pub fn empty() -> Self {
        Self { shards: Vec::new(), excluded_since: i64::MIN }
    }

    /// 排序去重（同分片取最大值；高水位「有则取最大」）。
    fn normalized(mut self) -> Self {
        self.shards.sort_unstable_by_key(|(s, _, _)| *s);
        self.shards.dedup_by(|a, b| {
            if a.0 == b.0 {
                b.1 = b.1.max(a.1);
                b.2 = match (b.2, a.2) {
                    (Some(x), Some(y)) => Some(x.max(y)),
                    (Some(x), None) | (None, Some(x)) => Some(x),
                    (None, None) => None,
                };
                true
            } else {
                false
            }
        });
        self
    }

    /// 请求携带的旧进度与本页覆盖的分片合并：本页没覆盖到的分片保留旧值；
    /// 覆盖到的分片水位线取「旧值与本页已交付最大值的较大者」（只前进不回退）。
    /// **历史高水位与排除线只由旧值承载**：排除线在签发时刻确立，此后入库的
    /// 回溯行必须可达，续拉不得把高水位抬高（否则新回溯行被误判为历史）。
    pub fn merged(self, fresh: &CursorProgress) -> CursorProgress {
        let mut out = self.shards.clone();
        for (shard, bound, _) in &fresh.shards {
            match out.iter_mut().find(|(s, _, _)| s == shard) {
                Some(slot) => slot.1 = slot.1.max(*bound),
                None => out.push((*shard, *bound, None)),
            }
        }
        out.sort_unstable_by_key(|(s, _, _)| *s);
        Self { shards: out, excluded_since: self.excluded_since }
    }

    /// 一行在游标续拉里是否可交付。判据按行标识与排除线的组合：
    ///
    /// - `local_id > 水位线`：未交付（水位线是「已交付集合的最大值」，排他保证
    ///   续拉无重放）；
    /// - 且**不是**「排除线界内的历史行」：`create_time ≤ 排除线` 且
    ///   `local_id ≤ 历史高水位` 的行在签发时刻已在库、已被窗口排除，不得冒出。
    ///   高水位为 `None` 时该分片签发时刻没有界内历史，时间早于排除线的行都是
    ///   其后入库的回溯行，照常可达。
    pub fn admits(&self, shard: u64, local_id: i64, create_time: i64) -> bool {
        if create_time <= self.excluded_since {
            let past = self
                .shards
                .iter()
                .find(|(s, _, _)| *s == shard)
                .and_then(|(_, _, h)| *h);
            match past {
                Some(high) if local_id <= high => return false,
                _ => {}
            }
        }
        local_id > self.bound_of(shard)
    }

    /// 某分片当前的水位线（游标线上没有 = 0）。
    pub fn bound_of(&self, shard: u64) -> i64 {
        self.shards
            .iter()
            .find(|(s, _, _)| *s == shard)
            .map(|(_, b, _)| *b)
            .unwrap_or(0)
    }

    /// 编码末段进度：各分片段 `<hex16><水位线>[-<高水位>]`（无排除线时高水位
    /// 恒缺席、不出 `-`），全局排除线段 `t<since>`（`i64::MIN` = 无排除，整段
    /// 省略）。`t` 取自十六进制字母表之外：分片段的前 16 字符必为十六进制，
    /// 以 `e` 等十六进制字符开头的分片代号不会误入排除线分支。空进度（无分片、
    /// 无排除）编 "0"。放不进 `max_len` 换算出的预算时返回 `None`——调用方
    /// （`CursorContext::encode`）升格为拒签，服务端不签发解不回的游标。
    fn encode(&self, max_len: usize) -> Option<String> {
        // 容量模型（与 decode 的 MAX_LEN 同源）：末段预算 = 总长上限减去
        // `v2.<nonce16>.<account16>.<registration≤20>.<generation≤10>.<session16>`
        // 的最大固定开销（6 个点分隔符已计入），再按 base64 的 4/3 膨胀折算
        // （向下取整，宁可少签不超签）。
        const IDENTITY_LEN: usize = 2 + 6 + 16 + 16 + 20 + 10 + 16;
        let budget = max_len
            .checked_sub(IDENTITY_LEN)?
            .checked_mul(3)?
            .checked_div(4)?;
        let mut out = String::new();
        for (i, (shard, watermark, high)) in self.shards.iter().enumerate() {
            if i > 0 {
                out.push('~');
            }
            out.push_str(&format!("{shard:016x}"));
            out.push_str(&watermark.to_string());
            if let Some(h) = high {
                out.push('-');
                out.push_str(&h.to_string());
            }
        }
        if self.excluded_since != i64::MIN {
            if !out.is_empty() {
                out.push('~');
            }
            out.push('t');
            out.push_str(&self.excluded_since.to_string());
        }
        if out.is_empty() {
            return Some("0".to_string());
        }
        (out.len() <= budget).then_some(out)
    }

    /// 解析末段进度。返回 `None` = 语法坏（400）。
    fn decode(text: &str) -> Option<Self> {
        if text == "0" {
            return Some(Self::empty());
        }
        let mut shards = Vec::new();
        let mut excluded_since = i64::MIN;
        for seg in text.split('~') {
            // 先按标记分流：排除线段以 `t` 开头（十六进制字母表之外），分片段
            // 前 16 字符必为十六进制——两者形态正交。
            if let Some(rest) = seg.strip_prefix('t') {
                if rest.is_empty() {
                    return None;
                }
                excluded_since = rest.parse().ok()?;
                continue;
            }
            if seg.len() <= SHARD_HEX || !seg.bytes().take(SHARD_HEX).all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let shard = u64::from_str_radix(&seg[..SHARD_HEX], 16).ok()?;
            let rest = &seg[SHARD_HEX..];
            // 高水位后缀 `-<n>`：十进制数字里没有 `-`，首次出现即分界。
            let (bound, high) = match rest.split_once('-') {
                Some((b, h)) => (b.parse().ok()?, Some(h.parse().ok()?)),
                None => (rest.parse().ok()?, None),
            };
            shards.push((shard, bound, high));
        }
        Some(Self { shards, excluded_since })
    }
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
        let progress = CursorProgress::with_exclusion(
            [(shard_code("message/message_0.db:Msg_abc"), 42)],
            [(shard_code("message/message_1.db:Msg_abc"), 7)],
            20,
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
    }

    #[test]
    fn multi_shard_progress_is_per_shard() {
        let a = shard_code("message/message_0.db:Msg_aaaa");
        let b = shard_code("message/message_1.db:Msg_aaaa");
        assert_ne!(a, b, "不同库的同名表必须拿到不同代号");
        let progress = CursorProgress::from_bounds([(a, 100), (b, 10)]);
        let encoded = ctx().encode(&progress).unwrap();
        let decoded = ctx().decode(&encoded).unwrap();
        assert_eq!(decoded, progress);
        // 低分片的新追加行必须可达：这正是只编会话最大 local_id 会丢掉的行。
        assert!(decoded.admits(b, 11, 1_700_000_100), "低分片新行 11 必须越过下界 10");
        assert!(!decoded.admits(b, 10, 1_700_000_100), "已交付行不得重放");
        assert!(decoded.admits(a, 101, 1_700_000_100));
    }

    #[test]
    fn same_shard_history_is_excluded_while_window_and_backfill_reach() {
        // 同分片的回溯泄漏形态（回归 `cursor_continuation_excludes_same_shard_history_but_reaches_later_backfill`
        // 钉端到端链路，这里钉判据本身）：分片 A 已交付 (lid=1, ts=30)，历史行
        // (lid=2, ts=10) 在初始窗口（since=20）之外。续拉必须排除历史行，同时
        // 不妨碍窗口内未交付行（lid 更小也可能）与其后入库的回溯行。
        let a = shard_code("db0:Msg_a");
        let progress = CursorProgress::with_exclusion([(a, 1)], [(a, 2)], 20);
        // 历史行：时间在排除线界内、行标识不高于高水位 ⇒ 定局。
        assert!(!progress.admits(a, 2, 10), "窗口外历史行不得经续拉冒出");
        // 已交付行不得重放。
        assert!(!progress.admits(a, 1, 30));
        // 单标量下界会永久滤掉的形态：历史行 lid=4 插在未交付窗口行 lid=2 之后。
        let progress = CursorProgress::with_exclusion([(a, 1)], [(a, 4)], 20);
        assert!(progress.admits(a, 2, 25), "未交付窗口行不得被历史高水位滤掉");
        assert!(!progress.admits(a, 4, 10), "历史行仍定局");
        // 排除线确立之后入库的回溯行（时间更早、行标识更大）照常可达。
        assert!(progress.admits(a, 5, 5), "其后入库的回溯行必须可达");
        // 无排除线的进度（纯增量链）不做历史排除。
        let plain = CursorProgress::from_bounds([(a, 1)]);
        assert!(plain.admits(a, 2, 10), "无排除线时不按时间排除");
    }

    #[test]
    fn exclusion_line_is_global_and_survives_merge() {
        let a = shard_code("db0:Msg_a");
        let b = shard_code("db1:Msg_b");
        // 首次 since=20 只覆盖 a；b 的历史行（ts=10）经历史高水位定局。
        let first = CursorProgress::with_exclusion([(a, 1)], [(b, 5)], 20);
        let encoded = ctx().encode(&first).unwrap();
        assert_eq!(ctx().decode(&encoded), Ok(first.clone()));
        assert!(!first.admits(b, 5, 10), "未覆盖分片的历史行定局");
        // 续拉覆盖 b：排除线保持（不随覆盖取消），b 的历史行仍定局。
        let next = first.merged(&CursorProgress::from_bounds([(b, 8)]));
        assert!(!next.admits(b, 5, 10), "覆盖不得取消历史排除");
        assert!(!next.admits(b, 8, 30), "已交付行（水位线本身）不得重放");
        assert!(next.admits(b, 9, 30), "水位线之后的行可达");
        assert!(next.admits(b, 9, 10), "排除线之后入库的回溯行可达");
        // 合并不抬高历史高水位：排除线确立后的新回溯行不得被误判为历史。
        let next = next.merged(&CursorProgress::from_bounds([(b, 9)]));
        assert!(next.admits(b, 10, 10), "合并后新回溯行仍可达");
    }

    #[test]
    fn e_prefixed_shard_codes_round_trip() {
        // 分片代号以十六进制 `e` 开头：排除线段用 `t` 标记（字母表之外），
        // 两者形态正交，往返必须无损。
        let code = 0xe123_4567_89ab_cdefu64;
        let progress = CursorProgress::with_exclusion([(code, 1)], [(code, 2)], 20);
        let encoded = ctx().encode(&progress).unwrap();
        assert_eq!(ctx().decode(&encoded), Ok(progress));
    }

    #[test]
    fn eighteen_shards_with_epoch_bounds_round_trip_within_the_limit() {
        // 多分片 + 十位时间戳曾是「签出即解不回」的形态：编码预算与解码上限
        // 同源后，这类游标必须仍能完整往返。
        let delivered: Vec<(u64, i64)> = (1..=18).map(|c| (c, 1_700_000_000)).collect();
        let history: Vec<(u64, i64)> = (1..=18).map(|c| (c, 1_690_000_000)).collect();
        let progress = CursorProgress::with_exclusion(delivered, history, 1_700_000_000);
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
            ctx().encode(&CursorProgress::from_bounds(bounds)),
            Err(CursorError::Overflow)
        );
    }

    #[test]
    fn merged_keeps_uncovered_shards_and_never_regresses() {
        let a = shard_code("db0:Msg_a");
        let b = shard_code("db1:Msg_b");
        let old = CursorProgress::from_bounds([(a, 100), (b, 10)]);
        // 本页只覆盖 a：b 的进度原样保留。
        let next = old.clone().merged(&CursorProgress::from_bounds([(a, 150)]));
        assert_eq!(next.bound_of(a), 150);
        assert_eq!(next.bound_of(b), 10);
        // 覆盖到的新分片进入进度线。
        let c = shard_code("db2:Msg_c");
        let next = next.merged(&CursorProgress::from_bounds([(c, 3)]));
        assert_eq!(next.bound_of(c), 3);
        assert_eq!(next.bound_of(a), 150, "合并不得回退已覆盖分片的进度");
        // 覆盖值小于旧值（理论上不该发生）时也不得回退。
        let next = next.merged(&CursorProgress::from_bounds([(a, 40)]));
        assert_eq!(next.bound_of(a), 150, "定局下界只前进不回退");
    }

    #[test]
    fn malformed_progress_segments_are_malformed() {
        let bad = ["0s", "s", "sabc0", "s0000000000000000", "szz000000000000005", "s0000000000000000x1"];
        for text in bad {
            assert!(CursorProgress::decode(text).is_none(), "{text:?} 必须判语法坏");
        }
    }

    #[test]
    fn malformed_cursor_is_a_bad_request() {
        for bad in ["", "!!!", "AAAA", "djI", "djIu", "djIuYS5i"] {
            assert_eq!(ctx().decode(bad), Err(CursorError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn syntactically_broken_identity_fields_are_malformed_not_reset() {
        use base64::Engine as _;
        let encode = |payload: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        // nonce 正确、账号/会话字段形态非法：这是语法坏（400），不是身份不符（409）。
        let cases = [
            encode(format!("v2.{}.xyz.3.7.{}.1", ctx().nonce, hex::encode([2u8; 8]))),
            encode(format!("v2.{}.{}.3.7.zz.1", ctx().nonce, hex::encode([1u8; 8]))),
            encode(format!("v2.{}.{}.3.7.{}.1", ctx().nonce, "aabb", "ccddeeff00112233")),
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
            (encode(format!("v2.{}.{}.{}.{}.{}.0", "ffffffffffffffff", hex::encode([1u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨进程"),
            (encode(format!("v2.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([9u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨账号"),
            (encode(format!("v2.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 3, 7, hex::encode([9u8; 8]))), "跨会话"),
            (encode(format!("v2.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 4, 7, hex::encode([2u8; 8]))), "注册代次"),
            (encode(format!("v2.{}.{}.{}.{}.{}.0", c.nonce, hex::encode([1u8; 8]), 3, 8, hex::encode([2u8; 8]))), "数据源世代"),
        ];
        for (cursor, what) in cases {
            assert!(
                matches!(c.decode(&cursor), Err(CursorError::Reset(_))),
                "{what} 必须判为需要重拉，而不是沿用或 400"
            );
        }
    }
}
