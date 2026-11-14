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
//! 末段 `progress` 编码**各分片的已交付上界**：`local_id` 只在 `source_shard`
//! （库文件:表名）内唯一，而同一会话的行可以来自多个分片表（真实库里 16 与 32
//! 位 md5 表共存；不同文件也可能各有一张同名 md5 表）。只编码会话最大
//! `local_id` 会把低分片的新追加行永远滤在「`local_id > 上界`」之外——且普通
//! 追加不推进数据源世代，客户端拿不到任何重置信号。形态：分片用其全名的
//! SHA-256 前 8 字节作紧凑代号，段与段之间 `~` 分隔，空进度（从头拉取）编 `0`；
//! 游标不跨进程存活（nonce 绑定本进程），不存在需要兼容的旧形态。
//!
//! 失效语义见 [`CursorError`]：语法坏 400；版本不认识、跨进程、跨账号、跨会话、
//! 代次不符一律 409 + `reason: cursor_reset_required`。**不静默退回时间模式**——
//! 那会让客户端以为续上了，实际上从头重放。

use crate::server::error::ApiError;

/// 游标总长上限：正常载荷不到 120 字节，超过这个数一定是垃圾或攻击。
const MAX_LEN: usize = 512;
const VERSION: &str = "v2";
/// 机器可读的失效原因：客户端据此**从头重拉**，而不是重试同一条游标。
pub const REASON_CURSOR_RESET: &str = "cursor_reset_required";

#[derive(Debug, PartialEq, Eq)]
pub enum CursorError {
    /// 语法坏：解不出来、字段数不对、数字或十六进制解析失败。这是**请求错误**（400）。
    Malformed,
    /// 语法没问题，但这条游标对当前请求已经失效（409）；字符串是给人看的原因。
    Reset(&'static str),
}

impl CursorError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            CursorError::Malformed => {
                ApiError::bad_request("cursor 无法解析：期望服务端签发的原样游标")
            }
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

    pub fn encode(&self, progress: &CursorProgress) -> String {
        let payload = format!(
            "{VERSION}.{}.{}.{}.{}.{}.{}",
            self.nonce,
            hex::encode(self.account),
            self.registration,
            self.generation,
            hex::encode(self.session),
            progress.encode(),
        );
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
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

/// 游标末段的进度：会话内每个分片各自的已交付上界（排他下界，严格大于才算新行），
/// 以及**未覆盖分片的时间排除线**。
///
/// 时间排除线解决「首次响应只覆盖部分分片」的泄漏：请求带 `since=20` 时，
/// 窗口外的分片 B（历史行时间 10）不在本次交付里，游标线上也就没有它的进度——
/// 续拉若对它从 0 起算，它的历史行会冒出来。排除线把「初始窗口对未覆盖分片的
/// 排除」钉进游标：续拉对线上没有进度线的分片仍按排除线过滤，带不带 since
/// 都不会重放窗口外历史。分片一旦被任何响应覆盖，排除线即被进度线取代
/// （进度线是更强的上界）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorProgress {
    /// `(分片代号, 该分片已交付的最大 local_id)`，按代号升序。
    shards: Vec<(u64, i64)>,
    /// 本游标的**排除线**：初始请求的 since（不排除历史 = `i64::MIN`）。
    /// 对 `shards` 里没有进度线的分片，续拉按它过滤 `create_time`。
    /// 排除线只收紧不放松：续拉携带新的 since（更晚）时取两者较大值。
    excluded_until: i64,
}

impl CursorProgress {
    /// 从会话里各分片的已交付上界构造。调用方只传「本次响应真正覆盖到的分片」；
    /// 游标线上没有的分片视为下界 0（首次拉取）——续拉请求携带旧进度，
    /// 未覆盖分片的进度由 [`CursorProgress::merged`] 保留。
    pub fn from_bounds(bounds: impl IntoIterator<Item = (u64, i64)>) -> Self {
        let mut shards: Vec<(u64, i64)> = bounds.into_iter().collect();
        shards.sort_unstable_by_key(|(s, _)| *s);
        shards.dedup_by(|a, b| a.0 == b.0);
        Self { shards, excluded_until: i64::MIN }
    }

    /// 带排除线的构造（初始请求带 since 时）。
    pub fn with_exclusion(
        bounds: impl IntoIterator<Item = (u64, i64)>,
        since: i64,
    ) -> Self {
        let mut out = Self::from_bounds(bounds);
        out.excluded_until = since;
        out
    }

    /// 空进度：全部行从 0 起算（「从头拉取」的语义位）。
    pub fn empty() -> Self {
        Self { shards: Vec::new(), excluded_until: i64::MIN }
    }

    /// 请求携带的旧进度与本页覆盖的分片合并：本页没覆盖到的分片保留旧值
    /// （进度不回退），覆盖到的取新值；排除线只收紧（取较大 since）。
    pub fn merged(self, fresh: &CursorProgress) -> CursorProgress {
        let mut out = self.shards.clone();
        for (shard, bound) in &fresh.shards {
            match out.iter_mut().find(|(s, _)| s == shard) {
                Some(slot) => slot.1 = *bound,
                None => out.push((*shard, *bound)),
            }
        }
        out.sort_unstable_by_key(|(s, _)| *s);
        let excluded_until = self.excluded_until.max(fresh.excluded_until);
        Self { shards: out, excluded_until }
    }

    /// 某分片当前的**时间下界**：进度线上的分片已无时间排除（None）；
    /// 线上没有的分片按排除线（`excluded_until`；`i64::MIN` = 不排除）。
    pub fn time_bound_for(&self, shard: u64) -> Option<i64> {
        let covered = self.shards.iter().any(|(s, _)| *s == shard);
        let excluded = self.excluded_until != i64::MIN;
        (excluded && !covered).then_some(self.excluded_until)
    }

    /// 一行是否已越过该行所在分片的进度线：严格大于（上界是「已交付集合的
    /// 最大值」，排他下界保证续拉无重放）。
    pub fn admits(&self, shard: u64, local_id: i64) -> bool {
        local_id > self.bound_of(shard)
    }

    /// 某分片当前的下界（游标线上没有 = 0）。
    pub fn bound_of(&self, shard: u64) -> i64 {
        self.shards
            .iter()
            .find(|(s, _)| *s == shard)
            .map(|(_, b)| *b)
            .unwrap_or(0)
    }

    fn encode(&self) -> String {
        // 排除线用 `e<since>` 后缀段表达（`i64::MIN` = 无排除，整段省略）；
        // 其余与分片进度线相同。空进度（无分片、无排除）仍编码 "0"——「从头拉取」
        // 的语义位，空串解不出来。
        let mut out = String::new();
        for (i, (shard, bound)) in self.shards.iter().enumerate() {
            if i > 0 {
                out.push('~');
            }
            out.push_str(&format!("{shard:016x}"));
            out.push_str(&bound.to_string());
        }
        if self.excluded_until != i64::MIN {
            if !out.is_empty() {
                out.push('~');
            }
            out.push('e');
            out.push_str(&self.excluded_until.to_string());
        }
        if out.is_empty() {
            return "0".to_string();
        }
        out
    }

    /// 解析末段进度。返回 `None` = 语法坏（400）。
    fn decode(text: &str) -> Option<Self> {
        if text == "0" {
            return Some(Self::empty());
        }
        let mut shards = Vec::new();
        let mut excluded_until = i64::MIN;
        for seg in text.split('~') {
            if let Some(rest) = seg.strip_prefix('e') {
                if rest.is_empty() {
                    return None;
                }
                excluded_until = rest.parse().ok()?;
                continue;
            }
            if seg.len() <= SHARD_HEX || !seg.bytes().take(SHARD_HEX).all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let shard = u64::from_str_radix(&seg[..SHARD_HEX], 16).ok()?;
            let bound: i64 = seg[SHARD_HEX..].parse().ok()?;
            shards.push((shard, bound));
        }
        Some(Self { shards, excluded_until })
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
        let encoded = ctx().encode(&CursorProgress::from_bounds([(shard_code("message/message_0.db:Msg_abc"), 42)]));
        assert_eq!(
            ctx().decode(&encoded),
            Ok(CursorProgress::from_bounds([(shard_code("message/message_0.db:Msg_abc"), 42)]))
        );
        // 标准字母表的 `+` / `/` 在 query 里会被改写，URL-safe 必须一个都不出现。
        assert!(!encoded.contains('+') && !encoded.contains('/'), "游标必须 URL-safe：{encoded}");
        assert!(!encoded.contains('='), "不留 padding，省一次转义：{encoded}");
    }

    #[test]
    fn empty_progress_round_trips_as_zero() {
        let encoded = ctx().encode(&CursorProgress::empty());
        assert_eq!(ctx().decode(&encoded), Ok(CursorProgress::empty()));
    }

    #[test]
    fn multi_shard_progress_is_per_shard() {
        let a = shard_code("message/message_0.db:Msg_aaaa");
        let b = shard_code("message/message_1.db:Msg_aaaa");
        assert_ne!(a, b, "不同库的同名表必须拿到不同代号");
        let progress = CursorProgress::from_bounds([(a, 100), (b, 10)]);
        let encoded = ctx().encode(&progress);
        let decoded = ctx().decode(&encoded).unwrap();
        assert_eq!(decoded, progress);
        // 低分片的新追加行必须可达：这正是只编会话最大 local_id 会丢掉的行。
        assert!(decoded.admits(b, 11), "低分片新行 11 必须越过下界 10");
        assert!(!decoded.admits(b, 10), "已交付行不得重放");
        assert!(decoded.admits(a, 101));
    }

    #[test]
    fn exclusion_line_survives_round_trip_and_merges_forward() {
        let a = shard_code("db0:Msg_a");
        let b = shard_code("db1:Msg_b");
        // 首次响应只覆盖 a（b 在 since 窗口外没有行）：排除线记下 since。
        let first = CursorProgress::with_exclusion([(a, 1)], 20);
        let encoded = ctx().encode(&first);
        assert_eq!(ctx().decode(&encoded), Ok(first.clone()));
        assert_eq!(first.time_bound_for(b), Some(20), "未覆盖分片按排除线过滤");
        assert_eq!(first.time_bound_for(a), None, "已覆盖分片由进度线接管");
        // 续拉覆盖到 b：排除线仍在（只收紧不放松），但 b 已被进度线接管。
        let next = first.merged(&CursorProgress::with_exclusion([(b, 1)], 30));
        assert_eq!(next.time_bound_for(b), None);
        assert_eq!(next.time_bound_for(a), None);
        // 无排除线的游标编码后不带 e 段（与旧载荷逐字节兼容）。
        let plain = CursorProgress::from_bounds([(a, 1)]);
        assert!(!ctx().encode(&plain).contains('e'), "无排除线不得引入 e 段");
    }

    #[test]
    fn merged_keeps_uncovered_shards_and_never_regresses() {        let a = shard_code("db0:Msg_a");
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
