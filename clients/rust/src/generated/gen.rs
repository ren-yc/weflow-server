#[allow(unused_imports)]
pub use progenitor_client::{ByteStream, ClientInfo, Error, ResponseValue};
#[allow(unused_imports)]
use progenitor_client::{encode_path, ClientHooks, OperationInfo, RequestBuilderExt};
/// Types used as operation parameters and responses.
#[allow(clippy::all)]
pub mod types {
    /**注册**被别的账号占位**（互锁）。

`occupied_by` / `occupied_status` 让客户端能记下「实际在跟哪个账号说话」，
而不是无限重试。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountConflict {
        pub occupied_by: ::std::string::String,
        pub occupied_status: AccountStatus,
        pub state: ::std::string::String,
        pub success: bool,
        pub wxid: ::std::string::String,
    }
    ///注销：**已成功解绑**。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountDeregistered {
        pub index_cleared: bool,
        pub previous_status: AccountStatus,
        pub purged_dirs: u64,
        pub purged_media: bool,
        pub state: ::std::string::String,
        pub success: bool,
        pub wxid: ::std::string::String,
    }
    ///注销：**本来就没有绑定**。刻意幂等 —— 重试已完成的注销得到 200 而不是错误。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountNotRegistered {
        pub index_cleared: bool,
        pub purged_dirs: u64,
        pub purged_media: bool,
        pub state: ::std::string::String,
        pub success: bool,
        pub wxid: ::std::string::String,
    }
    /**Coarse account phase for the unauthenticated `/health`.

`/health` needs no token, so it must not reveal which accounts exist on
this machine, how many there are, where their databases live, or why one
failed. The startup scan seeds one discovery entry per `xwechat_files`
profile directory, which makes even the *count* a disclosure. This enum has
no `AwaitingKey` variant at all, so leaking discovery results through
`/health` is a type error rather than a review item; the detail lives
behind the token-protected `GET /api/v1/accounts`.*/
    #[derive(
        ::serde::Deserialize,
        ::serde::Serialize,
        Clone,
        Copy,
        Debug,
        Eq,
        Hash,
        Ord,
        PartialEq,
        PartialOrd
    )]
    pub enum AccountPhase {
        #[serde(rename = "unregistered")]
        Unregistered,
        #[serde(rename = "indexing")]
        Indexing,
        #[serde(rename = "ready")]
        Ready,
        #[serde(rename = "error")]
        Error,
    }
    impl ::std::fmt::Display for AccountPhase {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
            match *self {
                Self::Unregistered => f.write_str("unregistered"),
                Self::Indexing => f.write_str("indexing"),
                Self::Ready => f.write_str("ready"),
                Self::Error => f.write_str("error"),
            }
        }
    }
    impl ::std::str::FromStr for AccountPhase {
        type Err = self::error::ConversionError;
        fn from_str(
            value: &str,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            match value {
                "unregistered" => Ok(Self::Unregistered),
                "indexing" => Ok(Self::Indexing),
                "ready" => Ok(Self::Ready),
                "error" => Ok(Self::Error),
                _ => Err("invalid value".into()),
            }
        }
    }
    impl ::std::convert::TryFrom<&str> for AccountPhase {
        type Error = self::error::ConversionError;
        fn try_from(
            value: &str,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            value.parse()
        }
    }
    impl ::std::convert::TryFrom<::std::string::String> for AccountPhase {
        type Error = self::error::ConversionError;
        fn try_from(
            value: ::std::string::String,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            value.parse()
        }
    }
    ///注册**受理**（或幂等命中：`already_ready` / `in_progress`）。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountRegistered {
        pub db_storage: ::std::string::String,
        pub state: ::std::string::String,
        pub status: AccountStatus,
        pub success: bool,
        pub wxid: ::std::string::String,
    }
    ///`AccountStateView`
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountStateView {
        /**字段**按字母序声明**：此前它是经 `json!` 序列化的（走 BTreeMap，键被排序），
而 struct 用声明序 —— 「换 DTO」时若不同步调整，响应里的键序会变。
快照的 `keys` 字段就是为抓这个而加的。

Resolved live-database directory (`<account>/db_storage`). Same field
name the registration endpoint echoes back.*/
        pub db_storage: ::std::string::String,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub error: ::std::option::Option<::std::string::String>,
        pub message_count: u64,
        pub state: AccountStatus,
        pub wxid: ::std::string::String,
    }
    /**One account's state-machine value, exposed via the token-protected
`GET /api/v1/accounts` and echoed by the registration endpoint.

NOT what `/health` reports: that endpoint is unauthenticated and carries the
coarser [`AccountPhase`] instead, which has no `AwaitingKey` variant.*/
    #[derive(
        ::serde::Deserialize,
        ::serde::Serialize,
        Clone,
        Copy,
        Debug,
        Eq,
        Hash,
        Ord,
        PartialEq,
        PartialOrd
    )]
    pub enum AccountStatus {
        #[serde(rename = "awaiting_key")]
        AwaitingKey,
        #[serde(rename = "indexing")]
        Indexing,
        #[serde(rename = "ready")]
        Ready,
        #[serde(rename = "error")]
        Error,
    }
    impl ::std::fmt::Display for AccountStatus {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
            match *self {
                Self::AwaitingKey => f.write_str("awaiting_key"),
                Self::Indexing => f.write_str("indexing"),
                Self::Ready => f.write_str("ready"),
                Self::Error => f.write_str("error"),
            }
        }
    }
    impl ::std::str::FromStr for AccountStatus {
        type Err = self::error::ConversionError;
        fn from_str(
            value: &str,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            match value {
                "awaiting_key" => Ok(Self::AwaitingKey),
                "indexing" => Ok(Self::Indexing),
                "ready" => Ok(Self::Ready),
                "error" => Ok(Self::Error),
                _ => Err("invalid value".into()),
            }
        }
    }
    impl ::std::convert::TryFrom<&str> for AccountStatus {
        type Error = self::error::ConversionError;
        fn try_from(
            value: &str,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            value.parse()
        }
    }
    impl ::std::convert::TryFrom<::std::string::String> for AccountStatus {
        type Error = self::error::ConversionError;
        fn try_from(
            value: ::std::string::String,
        ) -> ::std::result::Result<Self, self::error::ConversionError> {
            value.parse()
        }
    }
    ///注销：**互锁触发**（另一个账号持有绑定，它被完全不动地留下）。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountWxidMismatch {
        pub index_cleared: bool,
        pub occupied_by: ::std::string::String,
        pub occupied_status: AccountStatus,
        pub purged_dirs: u64,
        pub purged_media: bool,
        pub state: ::std::string::String,
        pub success: bool,
        pub wxid: ::std::string::String,
    }
    ///`GET /api/v1/accounts`。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct AccountsList {
        pub accounts: ::std::vec::Vec<AccountStateView>,
        pub success: bool,
    }
    ///ChatLab 信封头。`exportedAt` 是**墙钟**（每次请求都不同）——快照里靠时钟哨兵掩码。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct ChatlabHeader {
        #[serde(rename = "exportedAt")]
        pub exported_at: i64,
        pub generator: ::std::string::String,
        pub version: ::std::string::String,
    }
    ///本页出现过的发送者（去重）。`avatar` 无来源时是**空串**。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct ChatlabMember {
        #[serde(rename = "accountName")]
        pub account_name: ::std::string::String,
        pub avatar: ::std::string::String,
        #[serde(rename = "groupNickname")]
        pub group_nickname: ::std::string::String,
        #[serde(rename = "platformId")]
        pub platform_id: ::std::string::String,
    }
    ///ChatLab 面的消息项。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct ChatlabMessage {
        #[serde(rename = "accountName")]
        pub account_name: ::std::string::String,
        pub content: ::std::string::String,
        #[serde(rename = "groupNickname")]
        pub group_nickname: ::std::string::String,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub media: ::std::option::Option<MediaBrief>,
        #[serde(rename = "platformMessageId")]
        pub platform_message_id: ::std::string::String,
        #[doc = "**无引用时省略该键**（不是给 `null`）：规范把它列为可选 *string*，`null` 会让信任\n类型的读者拿到解析不了的值。三个面（原生面、消息面、拉取面）在这一键上同规。"]
        #[serde(
            rename = "replyToMessageId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub reply_to_message_id: ::std::option::Option<::std::string::String>,
        pub sender: ::std::string::String,
        pub timestamp: i64,
        #[serde(rename = "type")]
        pub type_: i64,
    }
    /**`GET /chatlab/messages`（消息面）。

**不带 `success`**：它输出的是数据信封，而 `success` 是「操作结果」的语言 —— 两者同时
出现时，读者无法判断 `count`/`page` 是否可信。翻页信息一律走 `page`，与发现面同规。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct ChatlabMessages {
        pub chatlab: ChatlabHeader,
        ///**本页条数**（不是总数）—— 总数不在这个面上表达，分页语义由 `page` 承担。
        pub count: u64,
        pub members: ::std::vec::Vec<ChatlabMember>,
        pub messages: ::std::vec::Vec<ChatlabMessage>,
        pub meta: ChatlabMeta,
        pub page: Page,
        pub talker: ::std::string::String,
    }
    ///会话元信息。`type` 是字符串；`ownerId` 未绑定时是空串（现状如此）。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct ChatlabMeta {
        #[serde(rename = "groupId")]
        pub group_id: ::std::string::String,
        pub name: ::std::string::String,
        #[serde(rename = "ownerId")]
        pub owner_id: ::std::string::String,
        pub platform: ::std::string::String,
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
    }
    /**联系人项。

**缺字段一律是空串，绝不是 `null`**。`store::Contact` 内部用 `Option<String>`（因为
`display_name()` 要区分「没有备注」与「备注是空串」），但**只有 JSON 边界**把它压平成
空串 —— 这个压平是契约的一部分，已在别处钉死。改回去会静默改变每个下游的判空逻辑。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct Contact {
        pub alias: ::std::string::String,
        #[serde(rename = "avatarUrl")]
        pub avatar_url: ::std::string::String,
        #[serde(rename = "displayName")]
        pub display_name: ::std::string::String,
        pub nickname: ::std::string::String,
        pub remark: ::std::string::String,
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
        pub username: ::std::string::String,
    }
    ///`GET|POST /api/v1/contacts`。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct Contacts {
        pub contacts: ::std::vec::Vec<Contact>,
        pub count: u64,
        #[serde(rename = "hasMore")]
        pub has_more: bool,
        pub success: bool,
        pub total: u64,
    }
    ///`DeleteApiV1AccountsWxidResponse`
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    #[serde(untagged)]
    pub enum DeleteApiV1AccountsWxidResponse {
        Deregistered(AccountDeregistered),
        NotRegistered(AccountNotRegistered),
        WxidMismatch(AccountWxidMismatch),
    }
    impl ::std::convert::From<AccountDeregistered> for DeleteApiV1AccountsWxidResponse {
        fn from(value: AccountDeregistered) -> Self {
            Self::Deregistered(value)
        }
    }
    impl ::std::convert::From<AccountNotRegistered> for DeleteApiV1AccountsWxidResponse {
        fn from(value: AccountNotRegistered) -> Self {
            Self::NotRegistered(value)
        }
    }
    impl ::std::convert::From<AccountWxidMismatch> for DeleteApiV1AccountsWxidResponse {
        fn from(value: AccountWxidMismatch) -> Self {
            Self::WxidMismatch(value)
        }
    }
    ///推送里的媒体元数据（**第三种形状**：无 `url` / `localPath` / `exported`）。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct EventMedia {
        ///建议文件名（含扩展名）。
        #[serde(rename = "fileName")]
        pub file_name: ::std::string::String,
        ///原文件的 md5；取不到时为 `null`（键保留）。
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub md5: ::std::option::Option<::std::string::String>,
        #[doc = "**可直接取字节的 id** —— 单段路由 `GET /api/v1/media/{id}`。\n\n**只在导出根下确有这个文件、且名字由内容摘要派生时才出现**（键随之消失，不是给\n`null`）：承诺是「出现即可取」，不是尽力而为 —— 按名取字节是跨会话解析的，平台给的\n名字（语音的 svr_id、视频的 DB 名）在别的会话里可能有同名异内容的文件。通告一个取不到\n的 id，调用方会拿到 404 并以为是服务坏了。"]
        #[serde(
            rename = "mediaId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub media_id: ::std::option::Option<::std::string::String>,
        ///媒体大类。
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
    }
    /**`message.new` 事件载荷。

**`media` 是第三种媒体形状**：只有 `type` / `fileName` / `md5` —— 推送里不含任何路径
与取字节用的键（字节走 REST 的 `media=1` 导出），也**永远不含**解密密钥。推一个取不到
的地址只会让客户端误以为有东西可拿。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct EventNew {
        pub content: ::std::string::String,
        /**事件名。帧头（`event:` 行）与载荷里各有一份，**不是**重复：客户端只解析
`data:` 行时也要能分辨类型。*/
        pub event: ::std::string::String,
        #[serde(
            rename = "groupName",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub group_name: ::std::option::Option<::std::string::String>,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub media: ::std::option::Option<EventMedia>,
        pub rawid: ::std::string::String,
        #[serde(rename = "sessionId")]
        pub session_id: ::std::string::String,
        #[serde(rename = "sessionType")]
        pub session_type: ::std::string::String,
        #[serde(rename = "sourceName")]
        pub source_name: ::std::string::String,
        pub timestamp: i64,
    }
    ///撤销事件。形状与 `message.new` 相同但**没有 `media`** —— 撤销的是消息，不是媒体。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct EventRevoke {
        pub content: ::std::string::String,
        pub event: ::std::string::String,
        #[serde(
            rename = "groupName",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub group_name: ::std::option::Option<::std::string::String>,
        pub rawid: ::std::string::String,
        #[serde(rename = "sessionId")]
        pub session_id: ::std::string::String,
        #[serde(rename = "sessionType")]
        pub session_type: ::std::string::String,
        #[serde(rename = "sourceName")]
        pub source_name: ::std::string::String,
        pub timestamp: i64,
    }
    /**水位基线/重基事件。客户端据此得知「从哪里继续拉」。

**什么时候发**：连接建立（基线）、订阅端落后（重基线）、索引重建、注销（水位归零）。

`generation` 在**注销**时递增：带着旧 `Last-Event-ID` 重连的客户端据此区分「注销后新账号
刚开始」（该丢弃本地状态重新拉）与「自己漏收了」（该补拉）。少了它这两种情况在协议上是同一
件事。它与新的通知面发的是**同一个**计数器。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct EventSync {
        ///事件名（`sync`）。
        pub event: ::std::string::String,
        ///事件基线代号，注销时递增。
        pub generation: i64,
        ///各表的水位线 —— 客户端从这里开始增量拉。
        pub watermarks: ::std::vec::Vec<WatermarkEntry>,
    }
    /**群成员项。字段缺失同样压平成**空串**（与 `contacts` 同规）。

`isOwner` 由 `chat_room.owner` 解析得出：**本页成员中恰为群主者为 `true`**；群主不在
本页、或缺 `chat_room`/owner 时全为 `false`（键恒保留）。`messageCount` 仅在
`includeMessageCounts=1` 时为真实值。删掉这些键会让下游的字段存在性判断失效。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct GroupMember {
        pub alias: ::std::string::String,
        #[serde(rename = "avatarUrl")]
        pub avatar_url: ::std::string::String,
        #[serde(rename = "displayName")]
        pub display_name: ::std::string::String,
        #[serde(rename = "groupNickname")]
        pub group_nickname: ::std::string::String,
        #[serde(rename = "isFriend")]
        pub is_friend: bool,
        #[serde(rename = "isOwner")]
        pub is_owner: bool,
        #[serde(rename = "messageCount")]
        pub message_count: i64,
        pub nickname: ::std::string::String,
        pub remark: ::std::string::String,
        pub wxid: ::std::string::String,
    }
    /**`GET /api/v1/group-members`。

成员集合是**名册 ∪ 发言人**：从未发过言的名册成员（潜水成员）也会出现，其 `messageCount`
为 0。这是有意的 —— 只列发言人会让「群里有谁」这个问题的答案取决于谁最近说过话。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct GroupMembers {
        #[serde(rename = "chatroomId")]
        pub chatroom_id: ::std::string::String,
        pub count: u64,
        ///名册与消息都在内存索引里，本请求不读盘、也不触发同步 ⇒ 恒为 `false`。
        #[serde(rename = "fromCache")]
        pub from_cache: bool,
        pub members: ::std::vec::Vec<GroupMember>,
        pub success: bool,
        /**索引构建完成时刻（**毫秒**）：客户端据此判断这份成员表有多旧。秒级会让同一秒内的
两次构建无法区分，而成员表的更新恰恰可能落在同一秒里。*/
        #[serde(rename = "updatedAt")]
        pub updated_at: i64,
    }
    /**`GET|POST /health` 与 `/api/v1/health`（**免鉴权**）。

刻意是标量：未鉴权方可访问，因此**不能**列出账号 —— 连数组长度都会泄露
「本机有几个账号、各自到哪一步」。账号身份与消息数走鉴权的 `/api/v1/accounts`。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct Health {
        pub account: AccountPhase,
        pub status: ::std::string::String,
        pub version: ::std::string::String,
    }
    /**一条消息的媒体**元数据**（拉取面与消息面同形）。

它**不是**「字节可取」的承诺：无媒体时整个键省略；`fileName` 只有在导出确实写出了本地
副本之后才是可取句柄（那时它是**实际导出文件名**），否则它只是这条消息自带的文件名。
`md5` 取不到时省略该键 —— 未导出不等于没有摘要。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct MediaBrief {
        #[serde(rename = "fileName")]
        pub file_name: ::std::string::String,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub md5: ::std::option::Option<::std::string::String>,
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
    }
    /**本页的导出状态。`exportPath` 是绝对路径（客户端用它找导出的文件），
`count` 是**成功导出**的条数 —— 不是本页消息数。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct MediaEnvelope {
        pub count: u64,
        pub enabled: bool,
        #[serde(rename = "exportPath")]
        pub export_path: ::std::string::String,
    }
    /**消息的媒体元数据 —— **原生面专用**（拉取面与消息面用 `MediaBrief`，SSE 用 `EventMedia`）。

- 未导出：`url` / `localPath` / `mediaId` **都不出现**，`exported` 也不出现；
- 导出成功：`url` 是根相对路径、`localPath` 是绝对路径，且多出 `exported: true`；
  其中**名字由内容摘要派生**的那些再多一个 `mediaId`（可直接喂给按名取字节的路由）；
  名字来自平台（视频的 DB 名回落）的那些**不给** `mediaId` —— 按名取字节是跨会话解析的，
  同名可能是别的会话的另一个文件。

`exported` 是**条件键**：不导出时它**不出现**而不是 `false`。客户端靠「键在不在」
判断字节可不可取 —— 改成恒出现会让这个判据失效。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct MediaObject {
        ///字母序使然：`exported` 排在 `fileName` 之前，这样输出与 `json!` 的现状一致。
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub exported: ::std::option::Option<bool>,
        ///文件名**始终是字符串**；没有名字时是空串（不是 `null`）。
        #[serde(rename = "fileName")]
        pub file_name: ::std::string::String,
        ///未导出时**省略**（不是空串）：空串会被读成「有路径、只是空的」。
        #[serde(
            rename = "localPath",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub local_path: ::std::option::Option<::std::string::String>,
        ///**取不到摘要时才省略**（不是给 `null`）：未导出不等于没有摘要。
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub md5: ::std::option::Option<::std::string::String>,
        ///可直接取字节的 id；只在**本次请求的导出批次**确实写出了摘要派生的本地文件时出现。
        #[serde(
            rename = "mediaId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub media_id: ::std::option::Option<::std::string::String>,
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
        ///根相对路径；未导出时**省略**。
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub url: ::std::option::Option<::std::string::String>,
    }
    ///原生面的消息项。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct MessageNative {
        ///`localType` 的高 32 位（appmsg 子类型）；无子类型时为 `null`。
        #[serde(
            rename = "appmsgSubtype",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub appmsg_subtype: ::std::option::Option<i64>,
        /**`localType` 的低位（基础类型）。两个半边分开给，免得每个消费方自己去拆
`(子类型 << 32) | 基础类型` 这种打包常量。*/
        #[serde(rename = "baseType")]
        pub base_type: i64,
        pub content: ::std::string::String,
        #[serde(rename = "createTime")]
        pub create_time: i64,
        #[serde(rename = "isSend")]
        pub is_send: i64,
        #[serde(rename = "localId")]
        pub local_id: i64,
        ///平台原始打包值 —— 下游已依赖它，保留。
        #[serde(rename = "localType")]
        pub local_type: i64,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub media: ::std::option::Option<MediaObject>,
        #[serde(rename = "parsedContent")]
        pub parsed_content: ::std::string::String,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub quote: ::std::option::Option<Quote>,
        #[serde(rename = "rawContent")]
        pub raw_content: ::std::string::String,
        #[doc = "**无引用时省略该键**（不是给 `null`）：下游若按「键在不在」判断引用关系，\n应改为「键在且非空」—— 这条与拉取面、消息面同规，三个面不再有形状差异。"]
        #[serde(
            rename = "replyToMessageId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub reply_to_message_id: ::std::option::Option<::std::string::String>,
        #[serde(rename = "senderName")]
        pub sender_name: ::std::string::String,
        #[serde(rename = "senderUsername")]
        pub sender_username: ::std::string::String,
        #[serde(rename = "serverId")]
        pub server_id: ::std::string::String,
        #[serde(rename = "sortSeq")]
        pub sort_seq: i64,
    }
    /**`GET|POST /api/v1/messages`（原生面）。

`media=1` 触发媒体导出，**单请求上限 200 项**——超出的部分保持未导出，再次请求续传。
这个闸门必须留在描述里：下游只能靠它规划分批策略，而它此前只存在于散文文档。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct MessagesNative {
        pub count: u64,
        #[serde(rename = "hasMore")]
        pub has_more: bool,
        pub media: MediaEnvelope,
        pub messages: ::std::vec::Vec<MessageNative>,
        pub success: bool,
        pub talker: ::std::string::String,
    }
    /**`/chatlab/push/messages` 的通知帧：**只带元信息，不带正文**。

规范对这条通道的定位是「仅通知：不假设事件可靠送达」—— 客户端收到后**去拉**那一页。
带正文会诱导调用方把它当数据源，而它并不保证送达；不带，语义就没有歧义。

`eventId` 与 `platformMessageId` 是**两个不同的号**：前者是事件通道自己的标识，后者是那条
消息在平台上的 id（拉取时用它定位）。

**`platformMessageId` 按事件类型给**：撤回帧带上（本仓事件里的 rawid 就是平台消息号），
`message.new` 仍为 `null` —— 新消息要把事件里的编号翻成平台消息号，得在**推送热路径**上
逐事件查一次索引，而规范里这个字段是**可选**的；定位它请用**拉取面**返回的同名字段，
「收到通知后去拉那一页」本来就是这条通道的用法。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct NotificationFrame {
        /**事件名。帧头（`event:` 行）与载荷里各有一份，**不是**重复：只解析 `data:` 行的客户端
也要能分辨类型。*/
        pub event: ::std::string::String,
        ///事件通道自己的标识。
        #[serde(rename = "eventId")]
        pub event_id: ::std::string::String,
        ///平台消息 id；取不到时为 `null`（键保留）。
        #[serde(
            rename = "platformMessageId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub platform_message_id: ::std::option::Option<::std::string::String>,
        ///所属会话。
        #[serde(rename = "sessionId")]
        pub session_id: ::std::string::String,
        ///事件时刻（秒）。
        pub timestamp: i64,
    }
    /**翻页信息。**`nextCursor` 始终出现**（排空时为 `null`），不要给它加
`skip_serializing_if`：客户端按「键在不在」判断「还有没有下一页」会读错。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct Page {
        #[serde(rename = "hasMore")]
        pub has_more: bool,
        #[serde(
            rename = "nextCursor",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub next_cursor: ::std::option::Option<::std::string::String>,
    }
    ///`PostApiV1AccountsResponse`
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    #[serde(untagged)]
    pub enum PostApiV1AccountsResponse {
        Registered(AccountRegistered),
        Conflict(AccountConflict),
    }
    impl ::std::convert::From<AccountRegistered> for PostApiV1AccountsResponse {
        fn from(value: AccountRegistered) -> Self {
            Self::Registered(value)
        }
    }
    impl ::std::convert::From<AccountConflict> for PostApiV1AccountsResponse {
        fn from(value: AccountConflict) -> Self {
            Self::Conflict(value)
        }
    }
    ///ChatLab Pull 信封：**顶层就是那五块**，没有 `success` / `count`。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct PullEnvelope {
        pub chatlab: ChatlabHeader,
        pub members: ::std::vec::Vec<ChatlabMember>,
        pub messages: ::std::vec::Vec<PullMessage>,
        pub meta: ChatlabMeta,
        pub sync: PullSync,
    }
    /**Pull 面的消息项。

**与消息面（`ChatlabMessage`）是两个 struct**，尽管字段目前一致：它们属于两个面 —— 一个
由 Pull 协议定义、一个由本服务的消息面定义，规范面加字段时不该顺带改到另一个面。
复用同一个 struct 会把两份契约绑在一起（这一批之前它们正是靠两个 struct 承载
`replyToMessageId` 的形状差异，现在差异消失了，分建的理由仍在）。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct PullMessage {
        #[serde(rename = "accountName")]
        pub account_name: ::std::string::String,
        pub content: ::std::string::String,
        #[serde(rename = "groupNickname")]
        pub group_nickname: ::std::string::String,
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub media: ::std::option::Option<MediaBrief>,
        /**本条消息的媒体**此刻就能取字节**时给出的句柄（`GET /api/v1/media/{id}`）。

「出现即可取」是承诺而非尽力而为：判据见 `chatlab_pull`（本会话导出目录里**确实
落盘**、且名字**由内容摘要派生**的那些才出现），取不到时**整个键省略**而不是给
`null` —— 通告一个必 404 的 id 比不给更坏：调用方会以为服务坏了，而它无从区分这两种情况。

它在**消息这一层**而不在 `media` 对象里：`media` 的键集由契约钉死为
`{type, fileName, md5}`（`media_shape_in_pull` 拒绝多余键），而 `fileName` 在未导出时
只是元数据名、可取时才等于句柄 —— 把两者塞进同一个键会让「有名字」与「取得到」混谈。
消息面（`ChatlabMessage`）**不加**这一键：那里导出后直接回填 `fileName`，两套表达同一件事
反而会分叉。*/
        #[serde(
            rename = "mediaId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub media_id: ::std::option::Option<::std::string::String>,
        #[serde(rename = "platformMessageId")]
        pub platform_message_id: ::std::string::String,
        #[doc = "**省略**（不是 `null`）：规范把它列为可选 *string*，`null` 会让信任类型的读者\n拿到解析不了的值。"]
        #[serde(
            rename = "replyToMessageId",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub reply_to_message_id: ::std::option::Option<::std::string::String>,
        pub sender: ::std::string::String,
        pub timestamp: i64,
        #[serde(rename = "type")]
        pub type_: i64,
    }
    /**翻页与水位。**两个游标都要原样回传**：`nextSince` 是排他下界、`nextOffset` 只用于
时间戳没能前进的退化情形，客户端自行推导会跳行。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct PullSync {
        #[serde(rename = "hasMore")]
        pub has_more: bool,
        #[serde(rename = "nextOffset")]
        pub next_offset: u64,
        #[serde(rename = "nextSince")]
        pub next_since: i64,
        pub watermark: i64,
    }
    ///引用（回复）的渲染信息。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct Quote {
        #[serde(rename = "accountName")]
        pub account_name: ::std::string::String,
        pub content: ::std::string::String,
        #[serde(rename = "platformMessageId")]
        pub platform_message_id: ::std::string::String,
        pub sender: ::std::string::String,
        #[serde(rename = "type")]
        pub type_: i64,
    }
    /**ChatLab 面的会话项。`type` 是字符串（`group` / `private`）。

`type` 只有两个取值（规范的枚举就这么大）—— 公众号与「其它」都归到 `private`：它们都是**一对
一的对话**，而规范没有第三个格子可放。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SessionChatlab {
        ///会话在数据源里的唯一标识，可直接用作拉取路径。
        pub id: ::std::string::String,
        ///最新消息时间戳（秒）。
        #[serde(rename = "lastMessageAt")]
        pub last_message_at: i64,
        /**成员数 —— **可选**：群名册的加载器拿得到才给（私聊、或名册缺失时**不出现这个键**）。

规范把它列为可选，而「没有名册」与「名册是空的」在下游是两件事：前者不该被读成 0。*/
        #[serde(
            rename = "memberCount",
            skip_serializing_if = "::std::option::Option::is_none"
        )]
        pub member_count: ::std::option::Option<u64>,
        ///消息总数（用于 ChatLab 展示预估量）。
        #[serde(rename = "messageCount")]
        pub message_count: u64,
        ///会话名称（群名/联系人名）。
        pub name: ::std::string::String,
        ///平台标识。
        pub platform: ::std::string::String,
        ///`group` / `private`。
        #[serde(rename = "type")]
        pub type_: ::std::string::String,
    }
    /**原生面的会话项。`type` 是**平台数值枚举**（weflow：0 私聊 / 1 群 / 2 公众号 / 3 其他；
qqflow 的取值含义不同），`sessionType` 是同一枚举的字符串形式 —— 下游应当用后者。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SessionNative {
        #[serde(rename = "displayName")]
        pub display_name: ::std::string::String,
        #[serde(rename = "lastTimestamp")]
        pub last_timestamp: i64,
        #[serde(rename = "messageCount")]
        pub message_count: u64,
        #[serde(rename = "sessionType")]
        pub session_type: ::std::string::String,
        #[doc = "**始终出现，无摘要时为 `null`** —— `Option` 且**不加** `skip_serializing_if`。\n这种「有键但值为空」与「没有这个键」的区别是契约的一部分，不要顺手统一。"]
        #[serde(skip_serializing_if = "::std::option::Option::is_none")]
        pub summary: ::std::option::Option<::std::string::String>,
        #[serde(rename = "type")]
        pub type_: i64,
        #[serde(rename = "unreadCount")]
        pub unread_count: i64,
        pub username: ::std::string::String,
    }
    /**`GET /chatlab/sessions`（Pull 形状的发现面）。

**与原生面的键集不同**，且 `type` 在这里是字符串 —— 两个面不可能共用一个 struct。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SessionsChatlab {
        pub count: u64,
        pub page: Page,
        pub sessions: ::std::vec::Vec<SessionChatlab>,
    }
    ///`GET /api/v1/sessions`（原生面）。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SessionsNative {
        pub count: u64,
        pub sessions: ::std::vec::Vec<SessionNative>,
        pub success: bool,
    }
    /**`/chatlab/push/messages` 的基线帧。

`generation` 在注销时递增（见 `server::GENERATION`）：带着旧 `Last-Event-ID` 重连的客户端
据此区分「注销后新账号刚开始」与「自己漏收了」—— 前者该丢弃本地状态重新拉，后者该补拉。
少了它，这两种情况在协议上是同一件事。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SyncFrame {
        ///事件名（`sync`）。
        pub event: ::std::string::String,
        ///基线代号。
        pub generation: i64,
        ///各表的水位线 —— 客户端从这里开始增量拉。
        pub watermarks: ::std::vec::Vec<WatermarkEntry>,
    }
    ///`POST /api/v1/sync`。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct SyncResult {
        #[serde(rename = "newMessages")]
        pub new_messages: u64,
        #[serde(rename = "revokeMessages")]
        pub revoke_messages: u64,
        pub success: bool,
    }
    ///一张表的水位。
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct WatermarkEntry {
        pub table: ::std::string::String,
        pub watermark: WatermarkValue,
    }
    /**水位三元组。三个都要：`local_id` 单独不够（同一秒可能有多条），`create_time` 单独
也不够（同秒内要靠 `sort_seq` 与 `local_id` 定序）。*/
    #[derive(::serde::Deserialize, ::serde::Serialize, Clone, Debug)]
    pub struct WatermarkValue {
        pub create_time: i64,
        pub local_id: i64,
        pub sort_seq: i64,
    }
    /// Error types.
    pub mod error {
        /// Error from a `TryFrom` or `FromStr` implementation.
        pub struct ConversionError(::std::borrow::Cow<'static, str>);
        impl ::std::error::Error for ConversionError {}
        impl ::std::fmt::Display for ConversionError {
            fn fmt(
                &self,
                f: &mut ::std::fmt::Formatter<'_>,
            ) -> Result<(), ::std::fmt::Error> {
                ::std::fmt::Display::fmt(&self.0, f)
            }
        }
        impl ::std::fmt::Debug for ConversionError {
            fn fmt(
                &self,
                f: &mut ::std::fmt::Formatter<'_>,
            ) -> Result<(), ::std::fmt::Error> {
                ::std::fmt::Debug::fmt(&self.0, f)
            }
        }
        impl From<&'static str> for ConversionError {
            fn from(value: &'static str) -> Self {
                Self(value.into())
            }
        }
        impl From<String> for ConversionError {
            fn from(value: String) -> Self {
                Self(value.into())
            }
        }
    }
}
#[derive(Clone, Debug)]
/**Client for weflow-server

Version: 0.8.0*/
pub struct Client {
    pub(crate) baseurl: String,
    pub(crate) client: reqwest::Client,
}
impl Client {
    /// Create a new client.
    ///
    /// `baseurl` is the base URL provided to the internal
    /// `reqwest::Client`, and should include a scheme and hostname,
    /// as well as port and a path stem if applicable.
    pub fn new(baseurl: &str) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let client = {
            let dur = ::std::time::Duration::from_secs(15u64);
            reqwest::ClientBuilder::new().connect_timeout(dur).timeout(dur)
        };
        #[cfg(target_arch = "wasm32")]
        let client = reqwest::ClientBuilder::new();
        Self::new_with_client(baseurl, client.build().unwrap())
    }
    /// Construct a new client with an existing `reqwest::Client`,
    /// allowing more control over its configuration.
    ///
    /// `baseurl` is the base URL provided to the internal
    /// `reqwest::Client`, and should include a scheme and hostname,
    /// as well as port and a path stem if applicable.
    pub fn new_with_client(baseurl: &str, client: reqwest::Client) -> Self {
        Self {
            baseurl: baseurl.to_string(),
            client,
        }
    }
}
impl ClientInfo<()> for Client {
    fn api_version() -> &'static str {
        "0.8.0"
    }
    fn baseurl(&self) -> &str {
        self.baseurl.as_str()
    }
    fn client(&self) -> &reqwest::Client {
        &self.client
    }
    fn inner(&self) -> &() {
        &()
    }
}
impl ClientHooks<()> for &Client {}
#[allow(clippy::all)]
impl Client {
    /**Sends a `GET` request to `/api/v1/accounts`

*/
    pub async fn get_api_v1_accounts<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::AccountsList>, Error<()>> {
        let url = format!("{}/api/v1/accounts", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_accounts",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `POST` request to `/api/v1/accounts`

*/
    pub async fn post_api_v1_accounts<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::PostApiV1AccountsResponse>, Error<()>> {
        let url = format!("{}/api/v1/accounts", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .post(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "post_api_v1_accounts",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**注销账号。

Sends a `DELETE` request to `/api/v1/accounts/{wxid}`

*/
    pub async fn delete_api_v1_accounts_wxid<'a>(
        &'a self,
        wxid: &'a str,
    ) -> Result<ResponseValue<types::DeleteApiV1AccountsWxidResponse>, Error<()>> {
        let url = format!(
            "{}/api/v1/accounts/{}", self.baseurl, encode_path(& wxid.to_string()),
        );
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .delete(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "delete_api_v1_accounts_wxid",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `GET` request to `/api/v1/contacts`

*/
    pub async fn get_api_v1_contacts<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::Contacts>, Error<()>> {
        let url = format!("{}/api/v1/contacts", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_contacts",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**成员集合是**名册 ∪ 发言人**：从未发过言的成员也会出现（`messageCount` 为 0）。

Sends a `GET` request to `/api/v1/group-members`

*/
    pub async fn get_api_v1_group_members<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::GroupMembers>, Error<()>> {
        let url = format!("{}/api/v1/group-members", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_group_members",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `GET` request to `/api/v1/health`

*/
    pub async fn get_api_v1_health<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::Health>, Error<()>> {
        let url = format!("{}/api/v1/health", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_health",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `POST` request to `/api/v1/health`

*/
    pub async fn post_api_v1_health<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::Health>, Error<()>> {
        let url = format!("{}/api/v1/health", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .post(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "post_api_v1_health",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**字节面（唯一入口）：只服务导出根下**摘要派生**的文件名；未导出的先经 `media=1`（每请求上限 200 项）导出。同名多命中时内容一致才服务、不一致给 404。

Sends a `GET` request to `/api/v1/media/{id}`

*/
    pub async fn get_api_v1_media_id<'a>(
        &'a self,
        id: &'a str,
    ) -> Result<ResponseValue<ByteStream>, Error<()>> {
        let url = format!(
            "{}/api/v1/media/{}", self.baseurl, encode_path(& id.to_string()),
        );
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self.client.get(url).headers(header_map).build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_media_id",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => Ok(ResponseValue::stream(response)),
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**原生/富数据形状：`limit` 默认 100、上限 10000；`media=1` 触发导出，**每请求最多导出 200 项**（超出的保持未导出，再请求续传）。ChatLab 形状走 `/chatlab/messages`。

Sends a `GET` request to `/api/v1/messages`

*/
    pub async fn get_api_v1_messages<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::MessagesNative>, Error<()>> {
        let url = format!("{}/api/v1/messages", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**SSE：重放缓冲 1000 条 / 600 秒；广播缓冲 1024；保活 25 秒（注释帧）。载荷为完整事件（老面形状）。

Sends a `GET` request to `/api/v1/push/messages`

*/
    pub async fn get_api_v1_push_messages<'a>(
        &'a self,
    ) -> Result<ResponseValue<ByteStream>, Error<()>> {
        let url = format!("{}/api/v1/push/messages", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self.client.get(url).headers(header_map).build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_push_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => Ok(ResponseValue::stream(response)),
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**会话列表（原生形状）：`limit` 默认 100、上限 10000；`offset` 翻页。ChatLab 形状走 `/chatlab/sessions`。

Sends a `GET` request to `/api/v1/sessions`

*/
    pub async fn get_api_v1_sessions<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::SessionsNative>, Error<()>> {
        let url = format!("{}/api/v1/sessions", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_sessions",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**游标拉取：`limit` 单页上限 5000。

Sends a `GET` request to `/api/v1/sessions/{id}/messages`

*/
    pub async fn get_api_v1_sessions_id_messages<'a>(
        &'a self,
        id: &'a str,
    ) -> Result<ResponseValue<types::PullEnvelope>, Error<()>> {
        let url = format!(
            "{}/api/v1/sessions/{}/messages", self.baseurl, encode_path(& id
            .to_string()),
        );
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_sessions_id_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `GET` request to `/api/v1/sync`

*/
    pub async fn get_api_v1_sync<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::SyncResult>, Error<()>> {
        let url = format!("{}/api/v1/sync", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_api_v1_sync",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `POST` request to `/api/v1/sync`

*/
    pub async fn post_api_v1_sync<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::SyncResult>, Error<()>> {
        let url = format!("{}/api/v1/sync", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .post(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "post_api_v1_sync",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**ChatLab 形状的消息面：`talker` 必填，`limit`/`offset`/`cursor`/`start`/`end`/`media`/`keyword`；`count` 是**本页条数**、消息**升序**、`page` 报告截断；`media=1` 真正执行导出（每请求上限 200 项）。

Sends a `GET` request to `/chatlab/messages`

*/
    pub async fn get_chatlab_messages<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::ChatlabMessages>, Error<()>> {
        let url = format!("{}/chatlab/messages", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_chatlab_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**SSE 通知面：只发元信息、不发正文；缓冲与保活同老面；基线帧带 `generation`。

Sends a `GET` request to `/chatlab/push/messages`

*/
    pub async fn get_chatlab_push_messages<'a>(
        &'a self,
    ) -> Result<ResponseValue<ByteStream>, Error<()>> {
        let url = format!("{}/chatlab/push/messages", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self.client.get(url).headers(header_map).build()?;
        let info = OperationInfo {
            operation_id: "get_chatlab_push_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => Ok(ResponseValue::stream(response)),
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Pull 形状的发现面：`keyword`/`limit`/`cursor` 分页；`count`/`page` 报告截断。

Sends a `GET` request to `/chatlab/sessions`

*/
    pub async fn get_chatlab_sessions<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::SessionsChatlab>, Error<()>> {
        let url = format!("{}/chatlab/sessions", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_chatlab_sessions",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Pull 面（与 `/api/v1/sessions/{id}/messages` 同一实现）：`limit` 单页上限 5000。

Sends a `GET` request to `/chatlab/sessions/{id}/messages`

*/
    pub async fn get_chatlab_sessions_id_messages<'a>(
        &'a self,
        id: &'a str,
    ) -> Result<ResponseValue<types::PullEnvelope>, Error<()>> {
        let url = format!(
            "{}/chatlab/sessions/{}/messages", self.baseurl, encode_path(& id
            .to_string()),
        );
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_chatlab_sessions_id_messages",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `GET` request to `/health`

*/
    pub async fn get_health<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::Health>, Error<()>> {
        let url = format!("{}/health", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_health",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**Sends a `POST` request to `/health`

*/
    pub async fn post_health<'a>(
        &'a self,
    ) -> Result<ResponseValue<types::Health>, Error<()>> {
        let url = format!("{}/health", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .post(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "post_health",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
    /**免鉴权：只描述形状，不含账号、路径与密钥。

Sends a `GET` request to `/openapi.json`

*/
    pub async fn get_openapi_json<'a>(
        &'a self,
    ) -> Result<
        ResponseValue<::serde_json::Map<::std::string::String, ::serde_json::Value>>,
        Error<()>,
    > {
        let url = format!("{}/openapi.json", self.baseurl,);
        let mut header_map = ::reqwest::header::HeaderMap::with_capacity(1usize);
        header_map
            .append(
                ::reqwest::header::HeaderName::from_static("api-version"),
                ::reqwest::header::HeaderValue::from_static(Self::api_version()),
            );
        #[allow(unused_mut)]
        let mut request = self
            .client
            .get(url)
            .header(
                ::reqwest::header::ACCEPT,
                ::reqwest::header::HeaderValue::from_static("application/json"),
            )
            .headers(header_map)
            .build()?;
        let info = OperationInfo {
            operation_id: "get_openapi_json",
        };
        self.pre(&mut request, &info).await?;
        let result = self.exec(request, &info).await;
        self.post(&result, &info).await?;
        let response = result?;
        match response.status().as_u16() {
            200u16 => ResponseValue::from_response(response).await,
            _ => Err(Error::UnexpectedResponse(response)),
        }
    }
}
/// Items consumers will typically use such as the Client.
pub mod prelude {
    #[allow(unused_imports)]
    pub use super::Client;
}
