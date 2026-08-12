# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## Project Overview

rezip is a Rust CLI tool that further reduces the size of already-compressed ZIP files by re-compressing with Zopfli.

## Commands

```bash
# Build and run
cargo build --release
cargo run -- [OPTIONS] <FILES>...

# Example usage
cargo run -- -l 264 -b 1048576 archive.zip
```

## Architecture

### Core Flow (recompress function)

1. Process each ZIP entry in parallel using rayon
2. File type-specific handling:
   - **Nested ZIP** (`50 4B 03 04`): recursively recompress
   - **PNG** (`89 50 4E 47...`): optimize with oxipng + Zopfli
   - Other files: recompress with Zopfli deflate
3. Compression strategy:
   - If Zopfli result is smaller → use it
   - If original compression is smaller than uncompressed → keep original (pass)
   - Otherwise → store uncompressed (Stored)
4. Reassemble entries and replace original if smaller

### Error Handling

- Error type aliased as `Box<dyn std::error::Error + Send + Sync>`
- Uses temporary files to protect originals

### Platform Notes

- Windows: UTF-8 console output setup with `winapi::wincon`
- `clippy::needless_return = "allow"` is set
