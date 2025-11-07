use std::{
    fs::{File, remove_file, rename},
    io::copy,
    path::PathBuf,
};

use clap::Parser;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use tempfile::{NamedTempFile, TempPath};
use zip::{
    CompressionMethod::{Deflated, Stored},
    ZipArchive, ZipWriter,
};

#[derive(Parser)]
struct Args {
    #[arg(short, default_value_t = 264)]
    level: i64,
    #[arg(short, default_value_t = 1 << 20)]
    buffer: usize,
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

fn main() -> Result<(), Error> {
    #[cfg(target_os = "windows")]
    unsafe {
        use winapi::um::{wincon::SetConsoleOutputCP, winnls::CP_UTF8};
        SetConsoleOutputCP(CP_UTF8);
    }

    let args = Args::parse();
    for path in args.files {
        println!("{}", &path.display());

        let result = (0..ZipArchive::new(File::open(&path)?)?.len())
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|i| -> Result<TempPath, Error> {
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

                    let options = file
                        .options()
                        .compression_method(Deflated)
                        .compression_level(Some(args.level))
                        .with_zopfli_buffer(Some(args.buffer));

                    let tmp = NamedTempFile::new()?;
                    {
                        let mut writer = ZipWriter::new(&tmp);
                        writer.start_file(file.name(), options)?;
                        copy(&mut file, &mut writer)?;
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
        let tmp = format!("{}.tmp", path.display());
        {
            let origin = ZipArchive::new(&origin)?;

            let mut writer = ZipWriter::new(File::create(&tmp)?);
            writer.set_comment(String::from_utf8_lossy(origin.comment()));

            for tmp in result {
                let mut archive = ZipArchive::new(File::open(tmp)?)?;
                let file = archive.by_index_raw(0)?;
                writer.raw_copy_file(file)?;
            }

            writer.finish()?;
        }

        let before = origin.metadata()?.len();
        let after = File::open(&tmp)?.metadata()?.len();
        if after < before {
            rename(tmp, &path)?;
            println!("{} {} -> {}", path.display(), before, after);
        } else {
            remove_file(tmp)?;
            println!("{} pass", path.display());
        }

        println!();
    }

    return Ok(());
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
