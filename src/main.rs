use std::{
    fs::File,
    io::{Read, Seek, Write, copy},
    num::NonZeroU8,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use anyhow::{Result, anyhow, ensure};
use clap::Parser;
use oxipng::{Deflaters, Options, optimize_from_memory};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use tempfile::{NamedTempFile, TempPath};
use zip::{
    CompressionMethod::{Deflated, Stored},
    ZipArchive, ZipWriter,
};

const ZIP_SIGNATURE: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Parser)]
struct Args {
    #[arg(short, default_value_t = 264)]
    level: i64,
    #[arg(short, default_value_t = 1 << 20)]
    buffer: usize,
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

fn main() -> Result<()> {
    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::{wincon::SetConsoleOutputCP, winnls::CP_UTF8};
        SetConsoleOutputCP(CP_UTF8);
    }

    let args = Args::parse();
    for path in &args.files {
        recompress(path, &args)?;
    }

    return Ok(());
}

fn merge_entries(
    output: File,
    comment: Box<[u8]>,
    archive_len: usize,
    rx: mpsc::Receiver<(usize, TempPath)>,
) -> Result<(ZipWriter<File>, usize)> {
    let mut zip = ZipWriter::new(output);
    zip.set_raw_comment(comment);

    let mut pending: Vec<Option<TempPath>> = (0..archive_len).map(|_| None).collect();
    let mut next = 0;
    for (index, entry_path) in rx {
        pending[index] = Some(entry_path);
        while let Some(entry_path) = pending.get_mut(next).and_then(Option::take) {
            let archive = ZipArchive::new(File::open(entry_path)?)?;
            ensure!(archive.len() == 1, "worker produced a multi-entry archive");
            zip.merge_archive(archive)?;
            next += 1;
        }
    }

    Ok((zip, next))
}

fn recompress(path: &Path, args: &Args) -> Result<()> {
    println!("{}", path.display());

    // Keep a stable, disk-backed snapshot so workers don't hold the full archive in RAM.
    let mut input = NamedTempFile::new()?;
    copy(&mut File::open(path)?, &mut input)?;
    let origin = ZipArchive::new(File::open(input.path())?)?;
    let archive_len = origin.len();
    let comment = origin.comment().to_vec().into_boxed_slice();

    let parent = path.parent().unwrap_or(Path::new("."));
    let (output, output_path) = NamedTempFile::new_in(parent)?.into_parts();
    let (tx, rx) = mpsc::channel::<(usize, TempPath)>();
    let writer = thread::spawn(move || merge_entries(output, comment, archive_len, rx));

    let jobs: Vec<_> = (0..archive_len).map(|index| (index, tx.clone())).collect();
    drop(tx);
    let input_path = input.path().to_path_buf();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        jobs.into_par_iter()
            .try_for_each(move |(i, tx)| -> Result<()> {
                let mut archive = ZipArchive::new(File::open(&input_path)?)?;

                // 디렉토리는 raw copy
                {
                    let file = archive.by_index_raw(i)?;
                    if file.is_dir() {
                        let tmp = NamedTempFile::new()?;
                        let mut writer = ZipWriter::new(&tmp);
                        writer.raw_copy_file(file)?;
                        writer.finish()?;

                        tx.send((i, tmp.into_temp_path()))
                            .map_err(|_| anyhow!("writer thread exited early"))?;
                        return Ok(());
                    }
                }

                // 파일 내용 추출 및 메타데이터 저장
                let mut file = archive.by_index(i)?;
                let file_name = file.name().to_owned();
                let compressed_size = file.compressed_size();
                let unix_mode = file.unix_mode();
                let last_modified = file.last_modified();
                let base_options = file.options();

                let mut extracted_file = NamedTempFile::new()?;
                copy(&mut file, &mut extracted_file)?;
                drop(file);
                drop(archive);

                // 시그니처 확인 및 중첩 압축 처리
                let mut signature = [0u8; 8];
                extracted_file.rewind()?;
                let bytes_read = extracted_file.read(&mut signature)?;

                if bytes_read >= 4 && signature[0..4] == ZIP_SIGNATURE {
                    // ZIP - 재귀적 재압축
                    if let Err(e) = recompress(extracted_file.path(), args) {
                        eprintln!("Failed to recompress nested archive {}: {}", file_name, e);
                    }
                } else if bytes_read == 8 && signature == PNG_SIGNATURE {
                    // PNG 최적화
                    let mut buffer = Vec::new();
                    extracted_file.rewind()?;
                    extracted_file.read_to_end(&mut buffer)?;

                    let mut opts = Options::max_compression();
                    opts.deflate = Deflaters::Zopfli {
                        iterations: NonZeroU8::new(args.level.clamp(1, 255) as u8).unwrap(),
                    };
                    if let Ok(optimized) = optimize_from_memory(&buffer, &opts)
                        && optimized.len() < buffer.len()
                    {
                        extracted_file.rewind()?;
                        extracted_file.as_file_mut().set_len(0)?;
                        extracted_file.write_all(&optimized)?;
                    }
                }
                let payload_size = extracted_file.as_file().metadata()?.len();

                // Zopfli로 재압축 시도
                let mut options = base_options
                    .compression_method(Deflated)
                    .compression_level(Some(args.level))
                    .with_zopfli_buffer(Some(args.buffer));

                if let Some(mode) = unix_mode {
                    options = options.unix_permissions(mode);
                }
                if let Some(time) = last_modified {
                    options = options.last_modified_time(time);
                }

                let tmp = NamedTempFile::new()?;
                let after_size = {
                    let mut writer = ZipWriter::new(&tmp);
                    writer.start_file(&file_name, options)?;
                    extracted_file.rewind()?;
                    copy(&mut extracted_file, &mut writer)?;
                    let mut archive = writer.finish_into_readable()?;
                    let file = archive.by_index_raw(0)?;
                    file.compressed_size()
                };

                // Zopfli 결과가 더 작으면 사용
                if after_size < compressed_size && after_size < payload_size {
                    println!(
                        "{} {}%",
                        file_name,
                        (100f64 - (after_size as f64 / compressed_size as f64) * 100f64).ceil()
                    );

                    tx.send((i, tmp.into_temp_path()))
                        .map_err(|_| anyhow!("writer thread exited early"))?;
                    return Ok(());
                }

                // 원본 압축이 더 작으면 raw copy (pass)
                if compressed_size <= payload_size {
                    let mut archive = ZipArchive::new(File::open(&input_path)?)?;
                    let file = archive.by_index_raw(i)?;

                    let tmp = NamedTempFile::new()?;
                    let mut writer = ZipWriter::new(&tmp);
                    writer.raw_copy_file(file)?;
                    writer.finish()?;

                    println!("{} pass", file_name);

                    tx.send((i, tmp.into_temp_path()))
                        .map_err(|_| anyhow!("writer thread exited early"))?;
                    return Ok(());
                }

                // 그 외에는 Stored로 저장
                let stored_options = base_options.compression_method(Stored);

                let tmp = NamedTempFile::new()?;
                let mut writer = ZipWriter::new(&tmp);
                writer.start_file(&file_name, stored_options)?;
                extracted_file.rewind()?;
                copy(&mut extracted_file, &mut writer)?;
                writer.finish()?;

                println!("{} stored", file_name);

                tx.send((i, tmp.into_temp_path()))
                    .map_err(|_| anyhow!("writer thread exited early"))?;
                return Ok(());
            })
    }))
    .unwrap_or_else(|panic| {
        Err(anyhow!(
            "a worker thread panicked: {}",
            panic_message(panic)
        ))
    });

    let writer_result = writer
        .join()
        .map_err(|panic| anyhow!("writer thread panicked: {}", panic_message(panic)))
        .and_then(|result| result);
    let (zip, received) = match (result, writer_result) {
        (_, Err(error)) => return Err(error),
        (Err(error), Ok(_)) => return Err(error),
        (Ok(()), Ok(done)) => done,
    };
    ensure!(
        received == archive_len,
        "writer received {received}/{archive_len} entries"
    );
    zip.finish()?;

    let before = input.as_file().metadata()?.len();
    let after = std::fs::metadata(&output_path)?.len();
    if after < before {
        output_path.persist(path)?;
        println!("{} {} -> {}", path.display(), before, after);
    } else {
        // tmp is automatically deleted when dropped
        println!("{} pass", path.display());
    }

    println!();
    Ok(())
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_owned();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    return "unknown panic".to_owned();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_out_of_order_worker_results_in_entry_order() -> Result<()> {
        let output = NamedTempFile::new()?;
        let (file, output_path) = output.into_parts();
        let (tx, rx) = mpsc::channel();
        let names = ["zero", "one", "two", "three"];
        for index in [2, 0, 3, 1] {
            let tmp = NamedTempFile::new()?;
            let mut writer = ZipWriter::new(&tmp);
            writer.start_file(names[index], zip::write::SimpleFileOptions::default())?;
            writer.write_all(names[index].as_bytes())?;
            writer.finish()?;
            tx.send((index, tmp.into_temp_path()))?;
        }
        drop(tx);

        let (writer, received) = merge_entries(file, Box::new([]), 4, rx)?;
        assert_eq!(received, 4);
        writer.finish()?;

        let mut archive = ZipArchive::new(File::open(output_path)?)?;
        let names: Vec<_> = (0..archive.len())
            .map(|index| archive.by_index(index).map(|entry| entry.name().to_owned()))
            .collect::<zip::result::ZipResult<_>>()?;
        assert_eq!(names, ["zero", "one", "two", "three"]);
        Ok(())
    }
}
