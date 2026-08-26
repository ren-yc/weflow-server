//! Media export pipeline tests: synthetic attach tree + hardlink/media
//! snapshot databases, exercising resolve→decode→export for all kinds.

mod common;

use std::path::Path;

use rusqlite::Connection;
use weflow_server::keystore::ImgCode;
use weflow_server::media::export::{export_batch, ExportCtx};
use weflow_server::media::{decrypt_dat, detect_format, DatFormat};

const TALKER: &str = "000000000000@chatroom";
const IMG_MD5: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const VID_MD5: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2";
const SVR_ID: i64 = 8523931155911769344;

fn aes128_ecb_encrypt(key: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let cipher = aes::Aes128::new(key.into());
    let mut out = pt.to_vec();
    for block in out.chunks_mut(16) {
        if block.len() == 16 {
            let mut b = [0u8; 16];
            b.copy_from_slice(block);
            cipher.encrypt_block((&mut b).into());
            block.copy_from_slice(&b);
        }
    }
    out
}

/// Build a V1 `.dat` sample: [magic][aesSize][xorSize][pad] + aes(pt) + raw + xor
fn build_v1_dat(jpeg: &[u8]) -> Vec<u8> {
    let (aes_pt, xor_part) = jpeg.split_at(16.min(jpeg.len()));
    let raw_part: &[u8] = &[];
    let mut data = vec![0x07, 0x08, 0x56, 0x31, 0x08, 0x07];
    data.extend_from_slice(&(aes_pt.len() as u32).to_le_bytes());
    data.extend_from_slice(&(xor_part.len() as u32).to_le_bytes());
    data.push(0);
    data.extend_from_slice(&aes128_ecb_encrypt(
        weflow_server::media::V1_FIXED_AES_KEY,
        aes_pt,
    ));
    // no raw segment in this sample: raw goes straight after AES
    data.extend_from_slice(raw_part);
    data.extend_from_slice(xor_part);
    data
}

fn setup(dir: &Path) -> ExportCtx {
    let account = dir.join("account");
    let session_md5 = format!("{:x}", {
        use md5::Digest;
        let mut h = md5::Md5::new();
        h.update(TALKER.as_bytes());
        h.finalize()
    });

    // --- image source: msg/attach/<md5(session)>/2025-08/Img/<md5>.dat (V1)
    let jpeg: Vec<u8> = [vec![0xFF, 0xD8, 0xFF, 0xE0], vec![0xAB; 40]].concat();
    let dat = build_v1_dat(&jpeg);
    assert_eq!(detect_format(&dat), Some(DatFormat::V1));
    let img_dir = account
        .join("msg")
        .join("attach")
        .join(&session_md5)
        .join("2025-08")
        .join("Img");
    std::fs::create_dir_all(&img_dir).unwrap();
    std::fs::write(img_dir.join(format!("{IMG_MD5}.dat")), &dat).unwrap();

    // --- video source: msg/video/2022-05/<md5>.mp4 (plaintext)
    let mp4 = [b' ', b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0, 0, 0].to_vec();
    let vdir = account.join("msg").join("video").join("2022-05");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::write(vdir.join(format!("{VID_MD5}.mp4")), &mp4).unwrap();

    // --- snapshot dbs
    let snap = dir.join("mirror").join(common::FAKE_WXID);
    let hl_dir = snap.join("hardlink");
    std::fs::create_dir_all(&hl_dir).unwrap();
    let hl = Connection::open(hl_dir.join("hardlink.db")).unwrap();
    hl.execute_batch(&format!(
        "CREATE TABLE image_hardlink_info_v4 (md5 TEXT, file_name TEXT);
         INSERT INTO image_hardlink_info_v4 VALUES ('{IMG_MD5}', '{IMG_MD5}.dat');
         CREATE TABLE video_hardlink_info_v4 (md5 TEXT, file_name TEXT);
         INSERT INTO video_hardlink_info_v4 VALUES ('{VID_MD5}', '{VID_MD5}.mp4');"
    ))
    .unwrap();

    let msg_dir = snap.join("message");
    std::fs::create_dir_all(&msg_dir).unwrap();
    let md = Connection::open(msg_dir.join("media_0.db")).unwrap();
    md.execute_batch(
        "CREATE TABLE VoiceInfo (svr_id INTEGER, voice_data BLOB, data_index TEXT DEFAULT '0');
         CREATE TABLE Name2Id (user_name TEXT);",
    )
    .unwrap();
    let silk: Vec<u8> = [vec![0x02], b"#!SILK_V3".to_vec(), vec![0x11, 0x22]].concat();
    md.execute(
        "INSERT INTO VoiceInfo (svr_id, voice_data, data_index) VALUES (?1, ?2, '0')",
        rusqlite::params![SVR_ID, silk],
    )
    .unwrap();

    ExportCtx {
        account_dir: account,
        snapshot_root: snap,
        export_dir: dir.join("api-media"),
        media_keys: Some(weflow_server::keystore::ImageKeys { aes: *weflow_server::media::V1_FIXED_AES_KEY, xor: 0 }),
        wxid: common::FAKE_WXID.to_string(),
    }
}

#[test]
fn exports_image_voice_video() {
    let dir = common::tmp_dir("mediaexport");
    let ctx = setup(&dir);

    let jobs = vec![
        (
            1i64,
            weflow_server::parser::MediaKind::Image,
            Some(IMG_MD5.to_string()),
            111i64,
            TALKER.to_string(),
        ),
        (
            2i64,
            weflow_server::parser::MediaKind::Voice,
            None,
            SVR_ID,
            TALKER.to_string(),
        ),
        (
            3i64,
            weflow_server::parser::MediaKind::Video,
            Some(VID_MD5.to_string()),
            333i64,
            TALKER.to_string(),
        ),
    ];
    let out = export_batch(&ctx, &jobs, 10);
    assert_eq!(out.len(), 3, "all three kinds exported: {out:?}");

    let img = out.get(&1).unwrap();
    assert_eq!(img.kind_dir, "images");
    let bytes = std::fs::read(&img.local_path).unwrap();
    assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]), "decoded jpeg magic");
    assert!(img.file_name.ends_with(".jpg"));

    let voice = out.get(&2).unwrap();
    assert_eq!(voice.kind_dir, "voices");
    let vb = std::fs::read(&voice.local_path).unwrap();
    assert!(vb.starts_with(b"#!SILK"), "status byte stripped: {vb:?}");

    let vid = out.get(&3).unwrap();
    assert_eq!(vid.kind_dir, "videos");
    assert!(vid.local_path.is_file());

    // idempotent: re-running yields the same paths without error
    let out2 = export_batch(&ctx, &jobs, 10);
    assert_eq!(out2.get(&1).unwrap().local_path, img.local_path);
}

#[test]
fn image_missing_source_is_none_but_others_succeed() {
    let dir = common::tmp_dir("mediaexport-miss");
    let ctx = setup(&dir);
    let jobs = vec![(
        9i64,
        weflow_server::parser::MediaKind::Image,
        Some("cccccccccccccccccccccccccccccccc".to_string()),
        1i64,
        TALKER.to_string(),
    )];
    let out = export_batch(&ctx, &jobs, 10);
    assert!(out.is_empty(), "unknown md5 must resolve to nothing");

    // decrypt sanity: the V1 sample really decodes through the public API too
    let src = std::fs::read(
        ctx.account_dir
            .join("msg")
            .join("attach")
            .join(format!("{:x}", {
                use md5::Digest;
                let mut h = md5::Md5::new();
                h.update(TALKER.as_bytes());
                h.finalize()
            }))
            .join("2025-08")
            .join("Img")
            .join(format!("{IMG_MD5}.dat")),
    )
    .unwrap();
    let (bytes, fmt) = decrypt_dat(&src, None, common::FAKE_WXID).unwrap();
    assert_eq!(fmt, DatFormat::V1);
    assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
}
