use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    process::Command,
};

use tempfile::tempdir;
use zip::{
    CompressionMethod::{Deflated, Stored},
    ZipArchive, ZipWriter,
    write::SimpleFileOptions,
};

fn run(path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rezip"))
        .args(["-l", "10", "-b", "4096"])
        .arg(path)
        .output()
        .expect("failed to run rezip")
}

#[test]
fn streams_completed_entries_in_original_order_without_losing_archive_metadata() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("archive.zip");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    writer.set_raw_comment(Box::new([0xff, 0, b'Z']));
    writer
        .add_directory("dir/", SimpleFileOptions::default())
        .unwrap();
    writer
        .start_file(
            "compressible.txt",
            SimpleFileOptions::default().compression_method(Stored),
        )
        .unwrap();
    writer.write_all(&vec![b'a'; 32 * 1024]).unwrap();

    let random: Vec<u8> = (0..32 * 1024)
        .scan(0x1234_5678_u32, |state, _| {
            *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Some((*state >> 24) as u8)
        })
        .collect();
    writer
        .start_file(
            "random.bin",
            SimpleFileOptions::default().compression_method(Stored),
        )
        .unwrap();
    writer.write_all(&random).unwrap();
    writer
        .start_file(
            "already.bin",
            SimpleFileOptions::default()
                .compression_method(Deflated)
                .compression_level(Some(10))
                .with_zopfli_buffer(Some(4096)),
        )
        .unwrap();
    writer.write_all(&vec![b'z'; 16 * 1024]).unwrap();
    writer.finish().unwrap();

    let original_raw = {
        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut entry = archive.by_index_raw(3).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        bytes
    };

    let output = run(&path);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let names: Vec<_> = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_owned())
        .collect();
    assert_eq!(
        names,
        ["dir/", "compressible.txt", "random.bin", "already.bin"]
    );
    assert_eq!(archive.comment(), [0xff, 0, b'Z']);
    assert!(archive.by_name("dir/").unwrap().is_dir());
    let mut compressible = archive.by_name("compressible.txt").unwrap();
    assert_eq!(compressible.compression(), Deflated);
    let mut restored_compressible = Vec::new();
    compressible
        .read_to_end(&mut restored_compressible)
        .unwrap();
    assert_eq!(restored_compressible, vec![b'a'; 32 * 1024]);
    drop(compressible);

    let mut random_entry = archive.by_name("random.bin").unwrap();
    assert_eq!(random_entry.compression(), Stored);
    let mut restored_random = Vec::new();
    random_entry.read_to_end(&mut restored_random).unwrap();
    assert_eq!(restored_random, random);
    drop(random_entry);

    let mut already = archive.by_index_raw(3).unwrap();
    let mut actual_raw = Vec::new();
    already.read_to_end(&mut actual_raw).unwrap();
    assert_eq!(actual_raw, original_raw);
}

#[test]
fn preserves_the_original_when_a_worker_cannot_read_an_entry() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corrupt.zip");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    writer
        .start_file(
            "broken.txt",
            SimpleFileOptions::default().compression_method(Stored),
        )
        .unwrap();
    writer.write_all(b"checksum protected payload").unwrap();
    writer.finish().unwrap();

    let data_start = {
        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        archive.by_index(0).unwrap().data_start()
    };
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(data_start)).unwrap();
    file.write_all(&[0]).unwrap();
    drop(file);
    let before = std::fs::read(&path).unwrap();

    let output = run(&path);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(path).unwrap(), before);
}
