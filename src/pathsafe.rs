//! Containment rules for filesystem path components built from untrusted input.
//!
//! Three different surfaces here turn caller- or database-supplied strings into
//! path components: the media export writer (`media::export::write_out`), the
//! exported-media route (`server::handlers::media`), the deregister purge
//! (`server::purge_exported_media`), and the SNS export writer
//! (`server::handlers::sns::export`). They shared no validation, so each had
//! drifted to its own subset of checks. This module is the single semantics.
//!
//! Why rejecting `.` / `..` / separators is NOT sufficient on Windows, which is
//! this service's primary platform (measured, not assumed):
//!
//! - Win32 strips **trailing dots and spaces** from a path component. So
//!   `sns-..` does not mean "parent" — it normalizes to a literal directory
//!   named `sns-`, which then consumes one level *downward*. A prefixed name
//!   like `format!("sns-{input}")` is therefore still traversable; the prefix
//!   buys nothing. Any component ending in `.` or ` ` must be rejected.
//! - Normalization is **lexical, in user space** (`GetFullPathName`, reached
//!   through `CreateFileW`). Intermediate directories need not exist for `..`
//!   to collapse, so a traversal that would fail with `ENOENT` on Unix still
//!   resolves on Windows.
//! - A component containing `:` opens an NTFS alternate data stream
//!   (`name.jpg:hidden`) or names a drive (`C:`), and carries no separator, so
//!   a naive separator-only filter lets it through.
//!
//! The rule this module enforces, and which callers should follow: derive a
//! safe name, then *assert containment* against the canonicalized root. Filtering
//! the input alone is what let the SNS export escape its directory.

use std::path::Path;

/// Longest single path component accepted. Real names here are md5 hex plus an
/// extension (`<32 hex>.jpg`), `voice_<i64>.silk`, or a wxid / `<id>@chatroom`,
/// all far below this; the bound exists to keep a hostile name from pushing the
/// joined path past the platform limit and turning containment into an IO error.
const MAX_SEGMENT: usize = 128;

/// True when `s` is safe to use as exactly one path component.
///
/// Rejects: empty, `.`, `..`, anything holding `/` `\` or `:`, control
/// characters (a NUL truncates the path at the syscall boundary), a trailing
/// dot or space (see the module docs — this is the Windows-specific one that
/// naive filters miss), and anything past [`MAX_SEGMENT`].
pub fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains(':')
        && !s.contains(|c: char| c.is_control())
        && !s.ends_with('.')
        && !s.ends_with(' ')
}

/// Fold an arbitrary string into one safe path component.
///
/// For names that are *derived* from untrusted input rather than matched against
/// it — the SNS export's filename scope, where the caller's `username` is a
/// display detail of the artifact name and rejecting it outright would break a
/// working request. Non-`[A-Za-z0-9-_]` bytes become `_`, so the result can hold
/// no separator, no `:`, and no dot at all — which also means it cannot end in
/// one, so the Windows trailing-dot rule is satisfied by construction rather than
/// by a follow-up trim. Only an empty result needs `fallback`.
///
/// Truncation is by `char`, never by byte, so a multi-byte UTF-8 name cannot be
/// cut mid-sequence.
pub fn slugify(s: &str, fallback: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if out.is_empty() {
        return fallback.to_string();
    }
    out
}

/// Assert that `path` really resolves inside `root`, as the last line of defense
/// after a name was derived.
///
/// Both sides are canonicalized so symlinks, `8.3` short names and the `\\?\`
/// verbatim prefix cannot produce a false match. `path` itself usually does not
/// exist yet (it is about to be written), so the check is on its parent — which
/// is also the component a traversal has to move, making it the right anchor.
///
/// Fails closed: an unresolvable root or parent returns false rather than
/// falling back to a lexical comparison against a non-canonical root, because
/// mixing a verbatim-prefixed canonical path with a raw one silently never
/// matches.
pub fn is_contained(root: &Path, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let (Ok(root), Ok(parent)) = (root.canonicalize(), parent.canonicalize()) else {
        return false;
    };
    parent.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_rejects_traversal_and_windows_quirks() {
        // ordinary names this service actually produces
        assert!(safe_segment("aabbccddeeff00112233445566778899.jpg"));
        assert!(safe_segment("voice_8100000000000000001.silk"));
        assert!(safe_segment("wxid_friend_a"));
        assert!(safe_segment("12345678@chatroom"));
        assert!(safe_segment("中文名字.png"), "non-ascii is fine, it carries no separator");

        // classic traversal
        assert!(!safe_segment(""));
        assert!(!safe_segment("."));
        assert!(!safe_segment(".."));
        assert!(!safe_segment("../evil"));
        assert!(!safe_segment("..\\evil"));
        assert!(!safe_segment("a/b"));
        assert!(!safe_segment("a\\b"));

        // Windows-specific: trailing dot/space are stripped by Win32, so these
        // normalize to a shorter name and can consume or move a level.
        assert!(!safe_segment("sns-.."), "trailing dots make this a level move");
        assert!(!safe_segment("evil."));
        assert!(!safe_segment("evil "));
        assert!(!safe_segment("..."), "resolves to the directory itself");

        // NTFS alternate data stream / drive-relative, no separator present
        assert!(!safe_segment("name.jpg:hidden"));
        assert!(!safe_segment("C:"));

        // control characters truncate at the syscall boundary
        assert!(!safe_segment("evil\0.jpg"));
        assert!(!safe_segment("evil\n.jpg"));

        assert!(!safe_segment(&"a".repeat(MAX_SEGMENT + 1)));
        assert!(safe_segment(&"a".repeat(MAX_SEGMENT)));
    }

    #[test]
    fn slugify_folds_every_traversal_shape() {
        assert_eq!(slugify("wxid_friend_a", "scope"), "wxid_friend_a");
        assert_eq!(slugify("all", "scope"), "all");
        // the PoC payload: every separator and dot folds away
        // 8 dots + 4 separators fold to 12 underscores
        assert_eq!(slugify(r"..\..\..\..\outside\pwned", "scope"), "____________outside_pwned");
        assert_eq!(slugify("../../etc/passwd", "scope"), "______etc_passwd");
        assert_eq!(slugify("..", "scope"), "__");
        // dots fold to '_' like everything else, so the result is inert rather
        // than empty; only a genuinely empty input needs the fallback
        assert_eq!(slugify("...", "scope"), "___");
        assert_eq!(slugify("", "scope"), "scope");
        // group ids keep their shape well enough to stay readable
        assert_eq!(slugify("12345678@chatroom", "scope"), "12345678_chatroom");
        // every output is usable as one component
        for probe in ["..", "../x", r"..\x", "a:b", "x.", "", "..."] {
            assert!(safe_segment(&slugify(probe, "scope")), "slug of {probe:?} must be a safe segment");
        }
    }

    #[test]
    fn slugify_truncates_by_char_not_byte() {
        let long = "中".repeat(100);
        let slug = slugify(&long, "scope");
        assert!(slug.chars().count() <= 64);
        assert!(safe_segment(&slug));
    }

    #[test]
    fn containment_holds_and_fails_closed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("pathsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("exports")).unwrap();

        let inside = root.join("exports").join("sns-all-20260829.json");
        assert!(is_contained(&root.join("exports"), &inside));

        // the traversal the SNS export used to allow
        let escaped = root
            .join("exports")
            .join(r"sns-..\..\..\outside\pwned-20260829.json");
        assert!(!is_contained(&root.join("exports"), &escaped));

        // unresolvable parent -> false, never a lexical fallback
        let missing = root.join("exports").join("nope").join("x.json");
        assert!(!is_contained(&root.join("exports"), &missing));

        let _ = std::fs::remove_dir_all(&root);
    }
}
