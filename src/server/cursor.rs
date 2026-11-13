//! 不透明进度游标 v2。
//!
//! 一条游标必须能自证四件事，否则「拿别人的游标续拉」会静默串数据：它是**本进程**
//! 编的（内存索引的行标识没有跨进程意义）、属于**这个账号**、属于**这个会话**，
//! 且编它时的**注册代次**与**数据源世代**都还是当前这一代。
//!
//! 载荷（点分，编码前全是 ASCII 安全字符）：
//! `v2.<nonce>.<account8>.<registration>.<generation>.<session8>.<localId>`，
//! 整段再做 URL-safe base64（无填充）。**不能用标准字母表**：`+` 在 query 里会被
//! 解成空格，客户端原样回传时就变成另一条游标。
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
    /// 语法坏：解不出来、字段数不对、数字解析失败。这是**请求错误**（400）。
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

    pub fn encode(&self, local_id: i64) -> String {
        let payload = format!(
            "{VERSION}.{}.{}.{}.{}.{}.{}",
            self.nonce,
            hex::encode(self.account),
            self.registration,
            self.generation,
            hex::encode(self.session),
            local_id,
        );
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    pub fn decode(&self, cursor: &str) -> Result<i64, CursorError> {
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
        let local_id = parts.next().ok_or(CursorError::Malformed)?;
        if parts.next().is_some() {
            return Err(CursorError::Malformed);
        }
        if nonce != self.nonce {
            return Err(CursorError::Reset("这条游标由另一个服务进程签发，重启后必须从头拉取"));
        }
        if account != hex::encode(self.account) {
            return Err(CursorError::Reset("这条游标属于另一个账号"));
        }
        if session != hex::encode(self.session) {
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
        local_id.parse().map_err(|_| CursorError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CursorContext {
        CursorContext { nonce: "0123456789abcdef", account: [1; 8], registration: 3, generation: 7, session: [2; 8] }
    }

    #[test]
    fn round_trip_and_url_safety() {
        let encoded = ctx().encode(42);
        assert_eq!(ctx().decode(&encoded), Ok(42));
        // 标准字母表的 `+` / `/` 在 query 里会被改写，URL-safe 必须一个都不出现。
        assert!(!encoded.contains('+') && !encoded.contains('/'), "游标必须 URL-safe：{encoded}");
        assert!(!encoded.contains('='), "不留 padding，省一次转义：{encoded}");
    }

    #[test]
    fn malformed_cursor_is_a_bad_request() {
        for bad in ["", "!!!", "AAAA", "djI", "djIu", "djIuYS5i"] {
            assert_eq!(ctx().decode(bad), Err(CursorError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn every_identity_mismatch_is_a_reset_not_a_silent_restart() {
        use base64::Engine as _;
        let encode = |payload: String| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        };
        let cases = [
            (encode("v1.0.5".into()), "v1 旧版本"),
            (encode(format!("v2.{}.{}.{}.{}.{}.1", "ffffffffffffffff", hex::encode([1u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨进程"),
            (encode(format!("v2.{}.{}.{}.{}.{}.1", ctx().nonce, hex::encode([9u8; 8]), 3, 7, hex::encode([2u8; 8]))), "跨账号"),
            (encode(format!("v2.{}.{}.{}.{}.{}.1", ctx().nonce, hex::encode([1u8; 8]), 3, 7, hex::encode([9u8; 8]))), "跨会话"),
            (encode(format!("v2.{}.{}.{}.{}.{}.1", ctx().nonce, hex::encode([1u8; 8]), 4, 7, hex::encode([2u8; 8]))), "注册代次"),
            (encode(format!("v2.{}.{}.{}.{}.{}.1", ctx().nonce, hex::encode([1u8; 8]), 3, 8, hex::encode([2u8; 8]))), "数据源世代"),
        ];
        for (cursor, what) in cases {
            assert!(
                matches!(ctx().decode(&cursor), Err(CursorError::Reset(_))),
                "{what} 必须判为需要重拉，而不是沿用或 400"
            );
        }
    }
}
