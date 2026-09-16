use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use multicore_release::{extract_verified_bundle, sha256_bytes};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

fn fake_pe() -> Vec<u8> {
    let mut bytes = vec![0_u8; 256];
    bytes[0..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&(0x80_u32).to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
    bytes
}

fn required_files() -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("MultiCore.exe".into(), fake_pe()),
        ("README.md".into(), b"readme".to_vec()),
        ("THIRD_PARTY_NOTICES.md".into(), b"notices".to_vec()),
        ("cores/mihomo.exe".into(), fake_pe()),
        ("cores/xray.exe".into(), fake_pe()),
        ("licenses/Xray-core-MPL-2.0.txt".into(), b"mpl".to_vec()),
        ("licenses/mihomo-GPL-3.0.txt".into(), b"gpl".to_vec()),
        ("runtime/multicore-daemon.exe".into(), fake_pe()),
        ("runtime/multicore-updater.exe".into(), fake_pe()),
        ("versions.json".into(), b"{}".to_vec()),
    ])
}

fn write_bundle(path: &Path, files: &BTreeMap<String, Vec<u8>>, sums: &BTreeMap<String, Vec<u8>>) {
    let file = File::create(path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        archive.start_file(name, options).unwrap();
        archive.write_all(bytes).unwrap();
    }
    let checksum_text = sums
        .iter()
        .map(|(name, bytes)| format!("{} *{}\n", sha256_bytes(bytes), name))
        .collect::<String>();
    archive.start_file("SHA256SUMS.txt", options).unwrap();
    archive.write_all(checksum_text.as_bytes()).unwrap();
    archive.finish().unwrap();
}

#[test]
fn valid_bundle_extracts_and_verifies_every_file() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let files = required_files();
    write_bundle(&archive, &files, &files);

    let destination = temp.path().join("out");
    let inventory = extract_verified_bundle(&archive, &destination).unwrap();
    assert_eq!(inventory.files(), 11);
    assert_eq!(
        std::fs::read(destination.join("MultiCore.exe")).unwrap(),
        fake_pe()
    );
}

#[test]
fn traversal_and_unlisted_files_are_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.insert("../escape.txt".into(), b"escape".to_vec());
    let sums = required_files();
    write_bundle(&archive, &files, &sums);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(error.to_string().contains("unsafe archive path"));
    assert!(!temp.path().join("escape.txt").exists());
}

#[test]
fn checksum_mismatch_is_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    let sums = files.clone();
    files.insert("README.md".into(), b"tampered".to_vec());
    write_bundle(&archive, &files, &sums);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
}

#[test]
fn missing_required_updater_is_rejected() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bundle.zip");
    let mut files = required_files();
    files.remove("runtime/multicore-updater.exe");
    write_bundle(&archive, &files, &files);

    let error = extract_verified_bundle(&archive, &temp.path().join("out")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("required bundle file is missing")
    );
}
