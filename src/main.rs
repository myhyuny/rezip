use std::{
    cmp::{max, min},
    fs::File,
    io::{Read, Seek, Write, copy},
    num::NonZeroU8,
    path::{Path, PathBuf},
};

use anyhow::Result;
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
fn recompress(path: &Path, args: &Args) -> Result<()> {
    println!("{}", &path.display());

    let result = (0..ZipArchive::new(File::open(&path)?)?.len())
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|i| -> Result<TempPath> {
            {
                let mut archive = ZipArchive::new(File::open(&path)?)?;
                let file = archive.by_index_raw(i)?;
                if file.is_dir() {
                    let tmp = NamedTempFile::new()?;
                    let mut writer = ZipWriter::new(&tmp);
                    writer.raw_copy_file(file)?;
                    writer.finish()?;

                    return Ok(tmp.into_temp_path());
                }
            }

            let (before_size, origin_size) = {
                let mut archive = ZipArchive::new(File::open(&path)?)?;
                let mut file = archive.by_index(i)?;

                let mut extracted_file = NamedTempFile::new()?;
                copy(&mut file, &mut extracted_file)?;

                let mut signature = [0u8; 8];
                extracted_file.rewind()?;
                let bytes_read = extracted_file.read(&mut signature)?;

                if bytes_read >= 4 && signature[0..4] == ZIP_SIGNATURE {
                    // ZIP
                    // Recursive recompression modifies the file in-place if successful
                    if let Err(e) = recompress(extracted_file.path(), args) {
                        eprintln!("Failed to recompress nested archive {}: {}", file.name(), e);
                    }
                } else if bytes_read == 8 && signature == PNG_SIGNATURE {
                    // PNG
                    let mut buffer = Vec::new();
                    extracted_file.rewind()?;
                    extracted_file.read_to_end(&mut buffer)?;

                    let mut opts = Options::max_compression();
                    opts.deflate = Deflaters::Zopfli {
                        iterations: NonZeroU8::new(min(max(args.level, 1), 255) as u8).unwrap(),
                    };
                    if let Ok(optimized) = optimize_from_memory(&buffer, &opts) {
                        if optimized.len() < buffer.len() {
                            extracted_file.rewind()?;
                            extracted_file.as_file_mut().set_len(0)?;
                            extracted_file.write_all(&optimized)?;
                        }
                    }
                }

                let mut options = file
                    .options()
                    .compression_method(Deflated)
                    .compression_level(Some(args.level))
                    .with_zopfli_buffer(Some(args.buffer));

                if let Some(mode) = file.unix_mode() {
                    options = options.unix_permissions(mode);
                }
                if let Some(time) = file.last_modified() {
                    options = options.last_modified_time(time);
                }

                let tmp = NamedTempFile::new()?;
                {
                    let mut writer = ZipWriter::new(&tmp);
                    writer.start_file(file.name(), options)?;
                    extracted_file.rewind()?;
                    copy(&mut extracted_file, &mut writer)?;
                    writer.finish()?;
                }

                let after_size = {
                    let mut archive = ZipArchive::new(&tmp)?;
                    let file = archive.by_index(0)?;
                    file.compressed_size()
                };
                if after_size < file.compressed_size() && after_size < file.size() {
                    println!(
                        "{} {}%",
                        file.name(),
                        (100f64 - (after_size as f64 / file.compressed_size() as f64) * 100f64)
                            .ceil()
                    );

                    return Ok(tmp.into_temp_path());
                }

                (file.compressed_size(), file.size())
            };

            if before_size < origin_size {
                let mut archive = ZipArchive::new(File::open(&path)?)?;
                let file = archive.by_index_raw(i)?;
                let file_name = file.name().to_owned();

                let tmp = NamedTempFile::new()?;
                let mut writer = ZipWriter::new(&tmp);
                writer.raw_copy_file(file)?;
                writer.finish()?;

                println!("{} pass", file_name);

                return Ok(tmp.into_temp_path());
            } else {
                let mut archive = ZipArchive::new(File::open(&path)?)?;
                let mut file = archive.by_index(i)?;

                let options = file.options().compression_method(Stored);

                let tmp = NamedTempFile::new()?;
                let mut writer = ZipWriter::new(&tmp);
                writer.start_file(file.name(), options)?;
                copy(&mut file, &mut writer)?;
                writer.finish()?;

                println!("{} stored", file.name());

                return Ok(tmp.into_temp_path());
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    let origin = File::open(&path)?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp = NamedTempFile::new_in(parent)?;
    {
        let origin = ZipArchive::new(&origin)?;

        let mut writer = ZipWriter::new(&tmp);
        writer.set_comment(String::from_utf8_lossy(origin.comment()));

        for entry in result {
            let mut archive = ZipArchive::new(File::open(entry)?)?;
            let file = archive.by_index_raw(0)?;
            writer.raw_copy_file(file)?;
        }

        writer.finish()?;
    }

    let before = origin.metadata()?.len();
    let after = tmp.as_file().metadata()?.len();
    if after < before {
        tmp.persist(&path)?;
        println!("{} {} -> {}", path.display(), before, after);
    } else {
        // tmp is automatically deleted when dropped
        println!("{} pass", path.display());
    }

    println!();
    Ok(())
}
