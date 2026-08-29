//! Message content parsing for WeChat 4.x.
//!
//! A message row carries `message_content` (XML for rich types, plain text
//! for type 1) and `compress_content` (zstd-compressed XML). The parser turns
//! a raw row into an API-friendly view: display text (with `[图片]`-style
//! placeholders), raw content, media hints, reply/quote info and revoke info.
//!
//! Design rule (inherited from qqflow-server): structured extraction first,
//! heuristic fallback, never panic on unknown shapes — degrade to display text.

/// Media kind surfaced to the API (`mediaType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Voice,
    Video,
    Emoji,
    File,
}

impl MediaKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Voice => "voice",
            MediaKind::Video => "video",
            MediaKind::Emoji => "emoji",
            MediaKind::File => "file",
        }
    }
}

/// Media hint extracted from a message (used by export/直服).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaHint {
    pub kind: MediaKind,
    /// Suggested file name (e.g. `<md5>.jpg`, `voice_<svrid>.silk`).
    pub file_name: String,
    pub md5: Option<String>,
    /// CDN aes key when present (needed to reconstruct the cache path).
    pub aes_key: Option<String>,
}

/// Quote: the referenced message copy embedded in an appmsg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteInfo {
    pub platform_message_id: String,
    pub sender: String,
    pub content: String,
    pub msg_type: i64,
}

/// Revocation info (local_type 10000/10002 with `revokemsg` xml).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeInfo {
    /// IDs usable to look up the withdrawn original message.
    pub msg_id: Option<String>,
    pub new_msg_id: Option<String>,
    /// Human text from the system message (`replacemsg`).
    pub replace_msg: Option<String>,
}

/// Full parsed view of one message row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMsg {
    /// Display text: plain content or placeholder (`[图片]` etc.).
    pub display: String,
    /// Raw content (XML or text) — the API `rawContent` field.
    pub raw_content: String,
    /// Strip to pure text when the raw content is XML (API `parsedContent`).
    pub parsed_text: String,
    pub media: Option<MediaHint>,
    /// serverId of the quoted message (API `replyToMessageId`).
    pub reply_to: Option<String>,
    pub quote: Option<QuoteInfo>,
    pub revoke: Option<RevokeInfo>,
}

/// WeChat local_type → display placeholder (WeFlow `getMessageDisplayContent`).
pub fn placeholder_for(local_type: i64) -> Option<&'static str> {
    match local_type {
        1 => None, // text: display the content itself
        3 => Some("[图片]"),
        34 => Some("[语音]"),
        43 => Some("[视频]"),
        47 => Some("[表情]"),
        42 => Some("[名片]"),
        48 => Some("[位置]"),
        49 => Some("[链接/文件]"),
        50 => Some("[视频号]"),
        10000 | 10002 => None, // system/revoke: parse the xml text
        _ => Some("[消息]"),
    }
}

/// Decode a message's content into XML/plain text.
///
/// WeChat 4.x stores long/XML payloads either in `compress_content`
/// (zstd-compressed) or directly in `message_content` — which may itself be a
/// raw zstd frame (`28 B5 2F FD …`). Both are handled here; anything else is
/// treated as lossy UTF-8 text.
///
/// `sender_username` is the row's own sender id, used to strip the group-chat
/// sender prefix (see [`strip_sender_prefix`]). Pass `""` when unknown.
pub fn decode_content(
    compress_content: Option<&[u8]>,
    message_content: Option<&[u8]>,
    sender_username: &str,
) -> Option<String> {
    if let Some(cc) = compress_content
        && let Some(xml) = zstd_decompress(cc)
    {
        return Some(strip_sender_prefix(xml, sender_username));
    }
    let mc = message_content?;
    if let Some(xml) = zstd_decompress(mc) {
        return Some(strip_sender_prefix(xml, sender_username));
    }
    // Plain text (`local_type == 1`) carries the same prefix and needs the same
    // treatment — it is not exempt just because it was never compressed.
    Some(strip_sender_prefix(
        String::from_utf8_lossy(mc).into_owned(),
        sender_username,
    ))
}

/// Strip WeChat's group-chat sender prefix (`<sender>:\n`) from a payload.
///
/// In `@chatroom` conversations WeChat prepends the sender's id and a newline to
/// the stored body, for both XML and plain text. Private chats have no prefix,
/// so stripping any `word:\n` opener unconditionally would eat real message
/// text. Two shapes are accepted instead:
///
/// - the first line is exactly `{sender_username}:` — an identity match against
///   the row's own sender, so it cannot collide with body text;
/// - the body starts with `<` — an XML payload never legitimately begins with a
///   `something:` line, and this keeps media hints working when the sender could
///   not be resolved.
///
/// The XML branch trims leading whitespace (media detection requires the payload
/// to start with `<`); the plain-text branch removes only the prefix and its
/// single newline, since leading blank lines can be part of the message.
fn strip_sender_prefix(body: String, sender_username: &str) -> String {
    let Some((first, rest)) = body.split_once('\n') else {
        return body;
    };
    let Some(id) = first.strip_suffix(':') else {
        return body;
    };
    if rest.trim_start().starts_with('<') {
        return rest.trim_start().to_string();
    }
    if !sender_username.is_empty() && id == sender_username {
        return rest.to_string();
    }
    body
}

fn zstd_decompress(data: &[u8]) -> Option<String> {
    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    let start = data.windows(4).take(16).position(|w| w == MAGIC)?;
    let payload = &data[start..];
    let bytes = zstd::stream::decode_all(payload).ok()?;
    String::from_utf8(bytes).ok()
}

/// Find an attribute value inside an XML-ish string: `name="value"`.
fn attr(xml: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = xml.find(&needle)? + needle.len();
    let end = xml[start..].find('"')? + start;
    Some(xml[start..end].to_string())
}

/// The `<appmsg>` subtype of a `local_type` 49 payload (1 文本, 5 链接,
/// 6 文件, 19 合并转发, 33/36 小程序, 51 视频号, 57 引用回复……).
///
/// 微信实际写的是元素形式 `<type>6</type>`；属性形式 `type="6"` 只出现在手写
/// 样例里。两种都接受，因为只用 `attr` 会漏掉所有真实的文件消息。
///
/// 只读访问器：ChatLab 类型映射用它而不用媒体提示 —— 后者的 `File` 判定为了
/// 导出目的故意放宽（`mmreader`/`webview` 也算），不能当作类型依据。
pub fn appmsg_type(xml: &str) -> Option<i64> {
    elem_text(xml, "type")
        .or_else(|| attr(xml, "type"))
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Extract the text of the first element with `name` (self-closing or not).
fn elem_text(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(xml[s..e].to_string())
}

/// Strip all XML tags, keeping inner text segments joined by a space.
fn strip_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut in_tag = false;
    let mut seg = String::new();
    for c in xml.chars() {
        match c {
            '<' => {
                in_tag = true;
                if !seg.trim().is_empty() {
                    out.push_str(seg.trim());
                    out.push(' ');
                }
                seg.clear();
            }
            '>' => in_tag = false,
            _ if !in_tag => seg.push(c),
            _ => {}
        }
    }
    if !seg.trim().is_empty() {
        out.push_str(seg.trim());
    }
    out.trim().to_string()
}

fn is_xml(s: &str) -> bool {
    s.trim_start().starts_with('<')
}

/// Parse a message row into the API view.
///
/// `local_id` is used to derive stable voice file names when no better
/// identifier exists.
pub fn parse_message(
    local_type: i64,
    server_id: i64,
    local_id: i64,
    content: &str,
) -> ParsedMsg {
    let raw = content.to_string();
    let parsed_text = if is_xml(content) { strip_tags(content) } else { content.to_string() };
    let display = match placeholder_for(local_type) {
        Some(p) => p.to_string(),
        None => match local_type {
            1 => parsed_text.clone(),
            10000 | 10002 => parse_system_text(content),
            _ => parsed_text.clone(),
        },
    };
    // For system/revoke messages the parsed text is the human-readable text.
    let parsed_text = if matches!(local_type, 10000 | 10002) {
        display.clone()
    } else {
        parsed_text
    };

    let mut media = None;
    let mut reply_to = None;
    let mut quote = None;
    let mut revoke = None;

    if is_xml(content) {
        match local_type {
            3 => {
                let md5 = attr(content, "md5").or_else(|| attr(content, "cdnthumbmd5"));
                let aes_key = attr(content, "aeskey");
                let ext = image_ext(content);
                let file_name = md5
                    .as_deref()
                    .map(|m| format!("{m}.{ext}"))
                    .unwrap_or_else(|| format!("img_{local_id}.{ext}"));
                media = Some(MediaHint {
                    kind: MediaKind::Image,
                    file_name,
                    md5,
                    aes_key,
                });
            }
            34 => {
                let svr = server_id.to_string();
                media = Some(MediaHint {
                    kind: MediaKind::Voice,
                    file_name: format!("voice_{svr}.silk"),
                    md5: attr(content, "md5"),
                    aes_key: attr(content, "aeskey"),
                });
            }
            43 => {
                let md5 = attr(content, "md5").or_else(|| attr(content, "cdnvideomd5"));
                let file_name = md5
                    .as_deref()
                    .map(|m| format!("{m}.mp4"))
                    .unwrap_or_else(|| format!("video_{local_id}.mp4"));
                media = Some(MediaHint {
                    kind: MediaKind::Video,
                    file_name,
                    md5,
                    aes_key: attr(content, "aeskey"),
                });
            }
            47 => {
                let md5 = attr(content, "md5");
                let file_name = md5
                    .as_deref()
                    .map(|m| format!("{m}.gif"))
                    .unwrap_or_else(|| format!("emoji_{local_id}.gif"));
                media = Some(MediaHint {
                    kind: MediaKind::Emoji,
                    file_name,
                    md5,
                    aes_key: None,
                });
            }
            49 => {
                // appmsg: file/link/miniapp; a refermsg makes it a quote
                let title = attr(content, "title").unwrap_or_default();
                let app_type = attr(content, "type").unwrap_or_default();
                if let Some(refer) = extract_refermsg(content) {
                    quote = Some(refer);
                    reply_to = quote
                        .as_ref()
                        .map(|q| q.platform_message_id.clone());
                }
                if app_type == "6" || content.contains("<mmreader") || content.contains("webview") {
                    let file_name = if title.is_empty() {
                        format!("file_{local_id}")
                    } else {
                        sanitize_file_name(&title)
                    };
                    media = Some(MediaHint {
                        kind: MediaKind::File,
                        file_name,
                        md5: None,
                        aes_key: None,
                    });
                }
            }
            10000 | 10002 => {
                revoke = extract_revoke(content);
            }
            _ => {}
        }
    }

    ParsedMsg {
        display: display.clone(),
        raw_content: raw,
        parsed_text,
        media,
        reply_to,
        quote,
        revoke,
    }
}

/// WeChat image extension by cache type hints (`type` attr: 2=png, 3=gif...).
fn image_ext(xml: &str) -> &'static str {
    match attr(xml, "type").as_deref() {
        Some("2") => "png",
        Some("3") => "gif",
        Some("4") => "webp",
        _ => "jpg",
    }
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_system_text(xml: &str) -> String {
    if let Some(r) = extract_revoke(xml) {
        if let Some(rm) = r.replace_msg {
            return rm;
        }
        return "撤回了一条消息".to_string();
    }
    // `<msg><sysmsg ...><sysmsgtemplate><content_template>...`
    if let Some(t) = elem_text(xml, "content_template") {
        return t;
    }
    if let Some(t) = elem_text(xml, "template") {
        return t;
    }
    // fallback: strip tags
    let text = strip_tags(xml);
    if text.is_empty() {
        xml.chars().take(64).collect()
    } else {
        text
    }
}

fn extract_revoke(xml: &str) -> Option<RevokeInfo> {
    let has = xml.contains("revokemsg")
        || xml.contains("replacemsg")
        || xml.contains("revoke")
        || xml.contains("撤回");
    if !has {
        return None;
    }
    let msg_id = elem_text(xml, "msgid").or_else(|| attr(xml, "msgid"));
    let new_msg_id = elem_text(xml, "newmsgid").or_else(|| attr(xml, "newmsgid"));
    let replace_msg = elem_text(xml, "replacemsg").or_else(|| attr(xml, "replacemsg"));
    if msg_id.is_none() && new_msg_id.is_none() && replace_msg.is_none() {
        return None;
    }
    Some(RevokeInfo {
        msg_id,
        new_msg_id,
        replace_msg,
    })
}

fn extract_refermsg(xml: &str) -> Option<QuoteInfo> {
    let s = xml.find("<refermsg>")? + "<refermsg>".len();
    let e = xml[s..].find("</refermsg>")? + s;
    let inner = &xml[s..e];
    let platform_message_id = elem_text(inner, "svrid")
        .or_else(|| attr(inner, "svrid"))
        .unwrap_or_default();
    let sender = elem_text(inner, "chatusr")
        .or_else(|| attr(inner, "chatusr"))
        .unwrap_or_default();
    // quoted content may be XML-in-XML (escaped); strip tags best-effort
    let content = elem_text(inner, "content").unwrap_or_default();
    let msg_type = elem_text(inner, "type")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    Some(QuoteInfo {
        platform_message_id,
        sender,
        content: strip_tags(&content),
        msg_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_message() {
        let p = parse_message(1, 1, 1, "你好");
        assert_eq!(p.display, "你好");
        assert_eq!(p.parsed_text, "你好");
        assert!(p.media.is_none());
    }

    #[test]
    fn image_placeholder_and_media() {
        let xml = r#"<msg><img hdLength="0" md5="aabbccddeeff00112233445566778899"/></msg>"#;
        let p = parse_message(3, 2, 2, xml);
        assert_eq!(p.display, "[图片]");
        let m = p.media.unwrap();
        assert_eq!(m.kind, MediaKind::Image);
        assert_eq!(m.file_name, "aabbccddeeff00112233445566778899.jpg");
        assert_eq!(m.md5.as_deref(), Some("aabbccddeeff00112233445566778899"));
    }

    #[test]
    fn voice_media() {
        let xml = r#"<msg><voicemsg voicelength="1200" fromusername="wxid_a" aeskey="ab"/></msg>"#;
        let p = parse_message(34, 123456, 3, xml);
        let m = p.media.unwrap();
        assert_eq!(m.kind, MediaKind::Voice);
        assert_eq!(m.file_name, "voice_123456.silk");
        assert_eq!(m.aes_key.as_deref(), Some("ab"));
    }

    #[test]
    fn revoke_message_extracts_ids() {
        let xml = r#"<sysmsg type="revokemsg"><revokemsg><msgid>9900000000000000001</msgid><newmsgid>9900000000000000002</newmsgid><replacemsg>对方撤回了一条消息</replacemsg></revokemsg></sysmsg>"#;
        let p = parse_message(10002, 0, 4, xml);
        let r = p.revoke.unwrap();
        assert_eq!(r.msg_id.as_deref(), Some("9900000000000000001"));
        assert_eq!(r.new_msg_id.as_deref(), Some("9900000000000000002"));
        assert!(p.display.contains("撤回"));
        assert_eq!(p.parsed_text, "对方撤回了一条消息");
    }

    #[test]
    fn quote_extraction() {
        let xml = r#"<msg><appmsg title="引用" type="57"><refermsg><type>1</type><svrid>777</svrid><chatusr>wxid_other</chatusr><content>你好</content></refermsg></appmsg></msg>"#;
        let p = parse_message(49, 5, 5, xml);
        assert_eq!(p.reply_to.as_deref(), Some("777"));
        let q = p.quote.unwrap();
        assert_eq!(q.sender, "wxid_other");
        assert_eq!(q.content, "你好");
    }

    #[test]
    fn zstd_compress_content_wins() {
        let xml = "<msg>zstd content</msg>";
        let compressed = zstd::stream::encode_all(xml.as_bytes(), 3).unwrap();
        let out = decode_content(Some(&compressed), Some(b"stale"), "");
        assert_eq!(out.as_deref(), Some(xml));
        // plain fallback
        assert_eq!(
            decode_content(Some(b"garbage-not-zstd"), Some(b"plain"), "").as_deref(),
            Some("plain")
        );
    }

    /// Group-chat plain text (`local_type == 1`) is stored uncompressed and
    /// carries the same `sender:\n` prefix as the XML payloads. Leaving it in
    /// leaks the sender id into the head of every group message body.
    #[test]
    fn plain_text_group_prefix_is_stripped() {
        let raw = "wxid_sender_a:\n你好啊".as_bytes();
        assert_eq!(
            decode_content(None, Some(raw), "wxid_sender_a").as_deref(),
            Some("你好啊")
        );
        // the prefix line ends with the sender id, so a wrong id must not match
        assert_eq!(
            decode_content(None, Some(raw), "wxid_someone_else").as_deref(),
            Some("wxid_sender_a:\n你好啊")
        );
        // unknown sender: no identity to match, so nothing is stripped
        assert_eq!(
            decode_content(None, Some(raw), "").as_deref(),
            Some("wxid_sender_a:\n你好啊")
        );
    }

    /// Private-chat bodies have no prefix, so a message that merely *opens* with
    /// `word:\n` must survive intact.
    #[test]
    fn plain_text_without_sender_match_is_untouched() {
        let raw = "注意:\n明天九点开会".as_bytes();
        assert_eq!(
            decode_content(None, Some(raw), "wxid_sender_a").as_deref(),
            Some("注意:\n明天九点开会")
        );
    }

    /// Only the prefix and its single newline go away — leading blank lines are
    /// part of the message body.
    #[test]
    fn plain_text_strip_preserves_leading_blank_lines() {
        let raw = b"wxid_sender_a:\n\n  indented";
        assert_eq!(
            decode_content(None, Some(raw), "wxid_sender_a").as_deref(),
            Some("\n  indented")
        );
    }

    /// The XML shape is accepted without an identity match (media hints need the
    /// payload to start with `<`), and leading whitespace is trimmed there.
    #[test]
    fn xml_prefix_is_stripped_without_sender() {
        let xml = "wxid_sender_a:\n  <msg><img md5=\"abc\" /></msg>";
        let compressed = zstd::stream::encode_all(xml.as_bytes(), 3).unwrap();
        assert_eq!(
            decode_content(Some(&compressed), None, "").as_deref(),
            Some("<msg><img md5=\"abc\" /></msg>")
        );
    }
}
/// Parsed SNS (朋友圈) media item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnsMediaHint {
    pub kind: &'static str,
    pub md5: Option<String>,
    pub url: String,
    pub thumb: Option<String>,
    pub width: i64,
    pub height: i64,
    pub token: Option<String>,
    pub key: Option<String>,
    pub enc_idx: Option<String>,
}


fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}


/// One `user_comment` block → (username, nickname, create_time, content).
/// Blocks inside `like_user_list` carry no content (they are likes).
fn parse_user_comment(block: &str) -> Option<crate::store::SnsComment> {
    let username = xml_text(block, "username").unwrap_or_default();
    if username.is_empty() {
        return None;
    }
    Some(crate::store::SnsComment {
        username,
        nickname: xml_text(block, "nickname").unwrap_or_default(),
        create_time: xml_text(block, "create_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        content: xml_text(block, "content").unwrap_or_default(),
    })
}

/// Parse one `SnsTimeLine` row into a feed view.
///
/// 4.x shape: `<SnsDataItem><TimelineObject><id/><username/><createTime/>
/// <contentDesc/>…<ContentObject><type>N</type><mediaList><media>…</mediaList>
/// </ContentObject></TimelineObject>
/// <LocalExtraInfo><tid/><nickname/><like_user_list><user_comment>…</user_comment>*
/// </like_user_list>[<CommentList><user_comment(with <content>)>…</CommentList>]`
pub fn parse_sns_feed(user_name: &str, tid: &str, xml: &str) -> crate::store::SnsFeed {
    let object_id = xml_text(xml, "id").unwrap_or_default();
    let nickname = xml_text(xml, "nickname").unwrap_or_default();
    let create_time = xml_text(xml, "createTime")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let content_desc = xml_text(xml, "contentDesc").unwrap_or_default();

    let mut media = Vec::new();
    for block in xml.split("<media>").skip(1) {
        let block = match block.find("</media>") {
            Some(end) => &block[..end],
            None => continue,
        };
        let mtype = xml_text(block, "type").unwrap_or_default();
        let kind = if mtype == "6" { "video" } else { "image" };
        let md5 = attr(block, "md5");
        let url = xml_elem_text(block, "url").unwrap_or_default();
        let thumb = xml_elem_text(block, "thumb");
        let token = attr(block, "token");
        let key = attr(block, "key");
        let enc_idx = attr(block, "enc_idx");
        let width = attr(block, "width")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let height = attr(block, "height")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if url.is_empty() && thumb.is_none() {
            continue;
        }
        media.push(SnsMediaHint { kind, md5, url, thumb, width, height, token, key, enc_idx });
    }

    // people: split user_comment blocks; those within like_user_list are likes
    let mut likes = Vec::new();
    let mut comments = Vec::new();
    let like_start = xml.find("<like_user_list>");
    let _like_end = like_start.map(|s| {
        xml[s..]
            .find("</like_user_list>")
            .map(|e| s + e)
            .unwrap_or(xml.len())
    });
    for block in xml.split("<user_comment").skip(1) {
        let block = match block.find("</user_comment>") {
            Some(end) => &block[..end],
            None => continue,
        };
        let Some(person) = parse_user_comment(block) else { continue };
        if !person.content.is_empty() {
            comments.push(person);
        } else {
            likes.push(crate::store::SnsPerson {
                username: person.username,
                nickname: person.nickname,
                create_time: person.create_time,
            });
        }
    }
    let comment_count = comments.len();

    let latitude = attr(xml, "latitude")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let longitude = attr(xml, "longitude")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let kind = if !media.is_empty() {
        if media.iter().any(|m| m.kind == "video") {
            "video"
        } else {
            "image"
        }
    } else if !content_desc.trim().is_empty() {
        "text"
    } else {
        "other"
    };

    crate::store::SnsFeed {
        feed_id: tid.to_string(),
        user_name: user_name.to_string(),
        object_id,
        nickname,
        create_time,
        content_desc: content_desc.trim().to_string(),
        kind,
        content_type: xml_text_between(xml, "<ContentObject><type>", "</type>")
            .unwrap_or_default(),
        media: media
            .into_iter()
            .map(|m| crate::store::SnsMedia {
                kind: m.kind,
                md5: m.md5,
                url: m.url,
                thumb: m.thumb,
                width: m.width,
                height: m.height,
                token: m.token,
                key: m.key,
                enc_idx: m.enc_idx,
            })
            .collect(),
        comment_count,
        likes,
        comments,
        latitude,
        longitude,
        raw_xml: xml.to_string(),
    }
}

/// Element text where the opening tag may carry attributes (`<url …>text</url>`).
fn xml_elem_text(xml: &str, tag_name: &str) -> Option<String> {
    let open_prefix = format!("<{tag_name}");
    let start = xml.find(&open_prefix)?;
    if start > 0 && !matches!(xml.as_bytes()[start - 1], b'<' | b' ' | b'\n' | b'\t' | b'\r') {
        // ensure this is a standalone tag, not a longer name (e.g. <media…)
    }
    let gt_rel = xml[start..].find('>')?;
    let gt = start + gt_rel;
    let close = format!("</{tag_name}>");
    let end_rel = xml[gt..].find(&close)?;
    Some(xml[gt + 1..gt + end_rel].to_string())
}
fn xml_text_between(xml: &str, open: &str, close: &str) -> Option<String> {
    let start = xml.find(open)? + open.len();
    let end = xml[start..].find(close)? + start;
    Some(xml[start..end].to_string())
}
#[cfg(test)]
mod sns_tests {
    use super::*;

    #[test]
    fn parses_real_shape_feed() {
        let xml = concat!(
            "<SnsDataItem><TimelineObject><id>14624933088173765273</id>",
            "<username>wxid_poster01</username><createTime>1743427883</createTime>",
            "<contentDesc>hello world</contentDesc>",
            "<ContentObject><type>1</type><mediaList>",
            "<media><id>m1</id><type>2</type>",
            "<thumb type=\"1\">http://shmmsns.qpic.cn/mmsns/X/150</thumb>",
            "<url type=\"1\" md5=\"2021f50af0b435101c0219d73dd2d44b\" key=\"k\" enc_idx=\"1\">http://shmmsns.qpic.cn/mmsns/X/0</url>",
            "<size width=\"2360\" height=\"528\" totalSize=\"28765\"/></media>",
            "</mediaList></ContentObject>",
            "<location latitude=\"31.2\" longitude=\"121.4\" poiScale=\"0\"/>",
            "</TimelineObject>",
            "<LocalExtraInfo><tid>14761639904696742450</tid><nickname>昵称甲</nickname>",
            "<like_user_list>",
            "<user_comment><comment_id>0</comment_id><username>wxid_liker01</username>",
            "<nickname>喜欢者甲</nickname><create_time>1759724613</create_time></user_comment>",
            "</like_user_list>",
            "<CommentList>",
            "<user_comment><username>wxid_commenter01</username><nickname>评论者乙</nickname>",
            "<create_time>1759724717</create_time><content>说得好</content></user_comment>",
            "</CommentList></LocalExtraInfo></SnsDataItem>"
        );
        let f = parse_sns_feed("wxid_poster01", "-3821810985535786343", xml);
        assert_eq!(f.object_id, "14624933088173765273");
        assert_eq!(f.nickname, "昵称甲");
        assert_eq!(f.create_time, 1743427883);
        assert_eq!(f.content_desc, "hello world");
        assert_eq!(f.content_type, "1");
        assert_eq!(f.kind, "image");
        assert_eq!(f.comment_count, 1);
        assert_eq!(f.media.len(), 1);
        assert_eq!(f.media[0].md5.as_deref(), Some("2021f50af0b435101c0219d73dd2d44b"));
        assert_eq!(f.media[0].width, 2360);
        assert_eq!(f.media[0].height, 528);
        assert_eq!(f.media[0].token.as_deref(), None);
        assert_eq!(f.likes.len(), 1);
        assert_eq!(f.likes[0].nickname, "喜欢者甲");
        assert_eq!(f.comments.len(), 1);
        assert_eq!(f.comments[0].content, "说得好");
        assert!((f.latitude - 31.2).abs() < 1e-6);
        assert!((f.longitude - 121.4).abs() < 1e-6);
        assert!(f.raw_xml.contains("TimelineObject"));
    }
}
