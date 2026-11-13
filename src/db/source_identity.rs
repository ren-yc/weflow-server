//! 数据源指纹：回答「同一个账号路径下，还是不是原来那些库文件」。
//!
//! 只取**身份类**线索：相对路径、创建时间，以及每个库文件前 16 字节的 SQLCipher
//! salt（WCDB 的页 1 以明文 salt 开头，每库独立随机）。内容类线索——文件大小、
//! mtime、页计数、change counter——**刻意不进指纹**：我方自己的写入也会改变它们，
//! 一旦纳入，每来一条新消息都会让游标失效，游标就等于永远不可用。
//!
//! `MetadataExt::file_index` 看起来更直接，但它仍是 `windows_by_handle` 不稳定特性，
//! 不能进生产；要拿文件索引得引入 Win32 依赖，当前边界下不值得。
//!
//! 代价是一条必须写进对外文档的盲区：**同一库文件的备份还原检测不到**
//! （salt 与创建时间都可能原样不变）。要覆盖它就必须引入 USN 加写入归因，
//! 那属于另一个量级的复杂度，不能靠「尽力检测」四个字含糊过去。

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// salt 长度：SQLCipher 页 1 的明文前缀。
const SALT_LEN: usize = 16;
/// 递归深度上限：账号目录下通常是 `message/`、`session/` 这类一层子目录。
const MAX_DEPTH: usize = 4;

/// 单个库文件的身份。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbIdentity {
    /// 文件创建时间（取不到时为 `None`）。换库时通常跟着变，还原同一文件时不变。
    pub created: Option<SystemTime>,
    pub salt: [u8; SALT_LEN],
}

/// 整个账号数据源的指纹。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub digest: [u8; 32],
    pub files: usize,
}

fn collect(
    dir: &Path,
    prefix: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = prefix.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect(&path, &rel, depth + 1, out)?;
        } else if ty.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
        {
            out.push((rel, path));
        }
    }
    Ok(())
}

fn read_salt(path: &Path) -> std::io::Result<[u8; SALT_LEN]> {
    let mut salt = [0u8; SALT_LEN];
    let mut file = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < SALT_LEN {
        match file.read(&mut salt[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(salt)
}

fn identity(path: &Path) -> std::io::Result<DbIdentity> {
    let meta = std::fs::metadata(path)?;
    Ok(DbIdentity { created: meta.created().ok(), salt: read_salt(path)? })
}

/// 账号目录下所有 `*.db` 的指纹。
///
/// 只包含身份类线索，因此**追加写入不会改变它**；换库、分片替换、重命名、
/// 增删库文件会改变它。
pub fn fingerprint(account_dir: &Path) -> std::io::Result<SourceFingerprint> {
    let mut files = Vec::new();
    collect(account_dir, Path::new(""), 0, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, path) in &files {
        let id = identity(path)?;
        // 相对路径只表达「是哪个文件」，不是内容；用原生字节避免 lossy 转换带来的歧义。
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        match id.created.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
            Some(d) => {
                hasher.update([1u8]);
                hasher.update(d.as_nanos().to_le_bytes());
            }
            None => hasher.update([0u8]),
        }
        hasher.update(id.salt);
    }

    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(SourceFingerprint { digest, files: files.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "weflow-srcid-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write_db(path: &Path, salt: [u8; SALT_LEN], payload: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建库目录");
        let mut bytes = salt.to_vec();
        bytes.extend_from_slice(payload);
        std::fs::write(path, bytes).expect("写库");
    }

    #[test]
    fn appends_do_not_change_the_fingerprint() {
        let dir = scratch("append");
        let db = dir.join("message/message_0.db");
        write_db(&db, [7u8; SALT_LEN], &[0u8; 64]);
        let before = fingerprint(&dir).expect("指纹");
        assert_eq!(before.files, 1);

        // 追加（大小变化）不能让指纹变化，否则每次入库都会让游标失效。
        let mut bytes = std::fs::read(&db).expect("读库");
        bytes.extend_from_slice(&[1u8; 512]);
        std::fs::write(&db, &bytes).expect("追加写");
        let after = fingerprint(&dir).expect("指纹");
        assert_eq!(before, after, "追加不是身份变化");
    }

    #[test]
    fn replacing_a_database_changes_the_fingerprint() {
        let dir = scratch("replace");
        let db = dir.join("message/message_0.db");
        write_db(&db, [7u8; SALT_LEN], &[0u8; 64]);
        let before = fingerprint(&dir).expect("指纹");

        // 换库：新文件的 salt 不同（真实替换里文件索引也常常不同）。
        write_db(&db, [9u8; SALT_LEN], &[0u8; 64]);
        let after = fingerprint(&dir).expect("指纹");
        assert_ne!(before, after, "换了库就不是同一个数据源");
    }

    #[test]
    fn adding_or_removing_a_shard_changes_the_fingerprint() {
        let dir = scratch("shards");
        let first = dir.join("message/message_0.db");
        write_db(&first, [1u8; SALT_LEN], &[0u8; 32]);
        let only_one = fingerprint(&dir).expect("指纹");

        let second = dir.join("message/message_1.db");
        write_db(&second, [2u8; SALT_LEN], &[0u8; 32]);
        let two = fingerprint(&dir).expect("指纹");
        assert_ne!(only_one, two, "新增分片必须改变指纹");
        assert_eq!((only_one.files, two.files), (1, 2));

        std::fs::remove_file(&second).expect("删分片");
        assert_eq!(fingerprint(&dir).expect("指纹"), only_one, "删回原来那个就还是原身份");
    }

    #[test]
    fn non_database_files_are_ignored() {
        let dir = scratch("ignore");
        write_db(&dir.join("message/message_0.db"), [3u8; SALT_LEN], &[0u8; 16]);
        let before = fingerprint(&dir).expect("指纹");
        std::fs::write(dir.join("message/message_0.db-wal"), [0u8; 128]).expect("写 WAL");
        std::fs::write(dir.join("notes.txt"), b"x").expect("写备注");
        assert_eq!(fingerprint(&dir).expect("指纹"), before);
    }
}
