//! Parse one rustdoc document and exit, so that peak RSS can be attributed.
//!
//! The benchmark binary is the wrong instrument for a memory question: it runs
//! every group, builds the 50 000-item index, and holds several documents alive
//! at once, so its `MaxRSS` is a figure for the harness rather than for a parse.
//! This does the one thing and stops, which makes `/usr/bin/time -l` mean what
//! it appears to mean.
//!
//! # Usage
//!
//! ```sh
//! # Measure a parse. A `.zst` path is expanded first, exactly as the client
//! # does with what docs.rs transfers.
//! /usr/bin/time -l cargo run --release --example parse_probe -- <path>
//!
//! # Write the generated ceiling document, then exit without parsing it. Kept
//! # here so that reproducing the measurement needs no second tool, and kept a
//! # separate invocation so that generating never lands in a measured run.
//! cargo run --release --example parse_probe -- --generate <path>
//! ```
//!
//! Every allocation the measured path makes is the client's. The probe itself
//! reads a file, prints one line, and returns.

use std::{error::Error, path::Path, process::ExitCode};

use crates_io_client::{DocIndex, docs, synthetic};

/// The allocator the server binary installs.
///
/// A memory figure taken against a different allocator than the one that ships
/// would answer a question nobody asked.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Expansion ceiling, far above any document this is pointed at.
const DECOMPRESS_LIMIT: usize = 64 * 1024 * 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("parse_probe: {error}");
            ExitCode::FAILURE
        },
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, path] if flag == "--generate" => generate(Path::new(path)),
        [path] => parse(Path::new(path)),
        _ => Err("usage: parse_probe <document> | parse_probe --generate <document>".into()),
    }
}

/// Write the generated document that reaches the item ceiling.
fn generate(path: &Path) -> Result<(), Box<dyn Error>> {
    let document = synthetic::rustdoc_document(synthetic::PATHS_FOR_CEILING);
    std::fs::write(path, &document)?;
    println!("generated {} bytes at {}", document.len(), path.display());
    Ok(())
}

/// The measured path: read, expand if compressed, index, report.
fn parse(path: &Path) -> Result<(), Box<dyn Error>> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split(['-', '.']).next())
        .unwrap_or("probe");

    let body = std::fs::read(path)?;
    let expanded = if path.extension().is_some_and(|extension| extension == "zst") {
        docs::decompress_rustdoc(name, &body, DECOMPRESS_LIMIT)?
    } else {
        body
    };

    let index = DocIndex::parse(name, &expanded)?;

    // Printing keeps the index alive to here, so the high-water mark covers a
    // built index rather than one the optimizer was free to discard.
    println!(
        "{name}: {} items, format_version {:?}, {} bytes in, truncated {}",
        index.len(),
        index.format_version(),
        expanded.len(),
        index.is_truncated()
    );
    Ok(())
}
