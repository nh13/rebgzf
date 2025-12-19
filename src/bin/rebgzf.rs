use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use rebgzf::{
    is_bgzf, validate_bgzf_streaming, validate_bgzf_strict, verify_bgzf, BgzfValidation,
    BgzfVerification, CompressionLevel, FormatProfile, ParallelTranscoder,
    SingleThreadedTranscoder, TranscodeConfig, Transcoder,
};

/// Format argument for CLI (maps to FormatProfile)
#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    /// Default encoding (fixed Huffman at level 1-3, dynamic at 4+)
    Default,
    /// FASTQ-optimized (implies level 6+ and record-aligned boundaries)
    Fastq,
    /// Auto-detect from file extension
    Auto,
}

impl FormatArg {
    fn to_format_profile(self) -> FormatProfile {
        match self {
            Self::Default => FormatProfile::Default,
            Self::Fastq => FormatProfile::Fastq,
            Self::Auto => FormatProfile::Auto,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "rebgzf")]
#[command(about = "Convert gzip files to BGZF format efficiently")]
#[command(version)]
struct Args {
    /// Input gzip file (use - for stdin)
    #[arg(short, long)]
    input: PathBuf,

    /// Output BGZF file (use - for stdout)
    #[arg(short, long, required_unless_present_any = ["check", "verify", "stats"])]
    output: Option<PathBuf>,

    /// Number of threads (0 = auto, 1 = single-threaded)
    #[arg(short = 't', long, default_value = "1")]
    threads: usize,

    /// Compression level (1-9): 1-3=fixed Huffman (fast), 4-6=dynamic, 7-9=dynamic+smart boundaries
    #[arg(short = 'l', long, default_value = "1", value_parser = clap::value_parser!(u8).range(1..=9))]
    level: u8,

    /// Input format profile for optimization
    #[arg(long, value_enum, default_value = "default")]
    format: FormatArg,

    /// BGZF block size (default: 65280)
    #[arg(long, default_value = "65280")]
    block_size: usize,

    /// Show verbose statistics
    #[arg(short, long)]
    verbose: bool,

    /// Quiet mode - suppress all output except errors
    #[arg(short, long)]
    quiet: bool,

    /// Output results as JSON (for scripting)
    #[arg(long)]
    json: bool,

    /// Check if input is BGZF and exit (0=BGZF, 1=not BGZF, 2=error)
    #[arg(long)]
    check: bool,

    /// Validate all BGZF blocks (slower, more thorough)
    #[arg(long)]
    strict: bool,

    /// Verify BGZF by decompressing and checking CRC32 (0=valid, 1=invalid, 2=error)
    #[arg(long)]
    verify: bool,

    /// Show file statistics without transcoding
    #[arg(long)]
    stats: bool,

    /// Force transcoding even if input is already BGZF
    #[arg(long)]
    force: bool,

    /// Show progress during transcoding (throughput display)
    #[arg(short = 'p', long)]
    progress: bool,

    /// Write GZI index file (for random access). If no path given, uses output.gzi
    #[arg(long, value_name = "PATH")]
    index: Option<Option<PathBuf>>,
}

/// Exit codes for --check mode
const EXIT_IS_BGZF: u8 = 0;
const EXIT_NOT_BGZF: u8 = 1;
const EXIT_ERROR: u8 = 2;

/// Exit codes for --verify mode
const EXIT_VERIFY_VALID: u8 = 0;
const EXIT_VERIFY_INVALID: u8 = 1;
// EXIT_ERROR (2) also used for verify errors

/// Progress tracking state shared between reader wrapper and progress thread
struct ProgressState {
    bytes_read: AtomicU64,
    total_size: Option<u64>,
    done: AtomicBool,
}

/// Reader wrapper that tracks bytes read for progress reporting
struct ProgressReader<R: Read> {
    inner: R,
    state: Arc<ProgressState>,
}

impl<R: Read> ProgressReader<R> {
    fn new(inner: R, state: Arc<ProgressState>) -> Self {
        Self { inner, state }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.state.bytes_read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Format bytes as human-readable string
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Spawn progress display thread
fn spawn_progress_thread(state: Arc<ProgressState>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut last_bytes = 0u64;
        let mut last_time = start;

        while !state.done.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));

            let bytes = state.bytes_read.load(Ordering::Relaxed);
            let now = Instant::now();
            let elapsed = now.duration_since(start);
            let delta_bytes = bytes.saturating_sub(last_bytes);
            let delta_time = now.duration_since(last_time);

            // Calculate throughput
            let throughput = if delta_time.as_secs_f64() > 0.0 {
                delta_bytes as f64 / delta_time.as_secs_f64() / 1_000_000.0
            } else {
                0.0
            };

            // Build progress line
            let progress_str = if let Some(total) = state.total_size {
                let pct = (bytes as f64 / total as f64 * 100.0).min(100.0);
                format!(
                    "\r{} / {} ({:.1}%) - {:.1} MB/s - {:.1}s elapsed",
                    format_bytes(bytes),
                    format_bytes(total),
                    pct,
                    throughput,
                    elapsed.as_secs_f64()
                )
            } else {
                format!(
                    "\r{} - {:.1} MB/s - {:.1}s elapsed",
                    format_bytes(bytes),
                    throughput,
                    elapsed.as_secs_f64()
                )
            };

            eprint!("{:<60}", progress_str);
            let _ = io::stderr().flush();

            last_bytes = bytes;
            last_time = now;
        }

        // Clear progress line
        eprint!("\r{:<60}\r", "");
        let _ = io::stderr().flush();
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Handle --check mode
    if args.check {
        return run_check_mode(&args);
    }

    // Handle --verify mode
    if args.verify {
        return run_verify_mode(&args);
    }

    // Handle --stats mode
    if args.stats {
        return run_stats_mode(&args);
    }

    // Normal transcoding mode - output is required
    let output_path = args.output.as_ref().expect("output required when not in check mode");

    // Determine I/O modes early (needed for index path logic)
    let is_stdin = args.input.to_str() == Some("-");
    let is_stdout = output_path.to_str() == Some("-");

    // Resolve format profile (Auto -> detected from extension)
    let format = args.format.to_format_profile().resolve(Some(&args.input));

    // Determine effective compression level
    // --format fastq implies at least level 6 for dynamic Huffman
    let compression_level = if format == FormatProfile::Fastq && args.level < 6 {
        CompressionLevel::Level6
    } else {
        CompressionLevel::from_level(args.level)
    };

    // Determine index output path
    let index_path: Option<PathBuf> = match &args.index {
        Some(Some(path)) => Some(path.clone()),
        Some(None) => {
            // --index without path: use output.gzi
            if !is_stdout {
                Some(output_path.with_extension("bgzf.gzi"))
            } else {
                eprintln!("Warning: --index requires an explicit path when output is stdout");
                None
            }
        }
        None => None,
    };

    let config = TranscodeConfig {
        block_size: args.block_size,
        compression_level,
        format,
        num_threads: args.threads,
        strict_bgzf_check: args.strict,
        force_transcode: args.force,
        build_index: index_path.is_some(),
        ..Default::default()
    };

    // Check for BGZF fast-path (only for file inputs, not stdin)
    if !config.force_transcode && !is_stdin {
        let mut file = BufReader::new(File::open(&args.input)?);

        let is_valid_bgzf = if config.strict_bgzf_check {
            let validation = validate_bgzf_strict(&mut file)?;
            if args.verbose && validation.is_valid_bgzf {
                if let Some(blocks) = validation.block_count {
                    eprintln!("Input is valid BGZF ({} blocks)", blocks);
                }
            }
            validation.is_valid_bgzf
        } else {
            is_bgzf(&mut file)?
        };

        if is_valid_bgzf {
            // Fast-path: copy directly
            if args.verbose {
                eprintln!("Input is already BGZF, copying directly");
            }

            // Seek back to start
            file.seek(SeekFrom::Start(0))?;

            let start = std::time::Instant::now();
            let bytes_copied = if is_stdout {
                let mut stdout = io::stdout().lock();
                io::copy(&mut file, &mut stdout)?
            } else {
                let mut output = BufWriter::new(File::create(output_path)?);
                io::copy(&mut file, &mut output)?
            };
            let elapsed = start.elapsed();

            if args.verbose {
                eprintln!("Copy complete:");
                eprintln!("  Bytes copied:     {}", bytes_copied);
                eprintln!("  Time:             {:.2?}", elapsed);
                eprintln!(
                    "  Throughput:       {:.1} MB/s",
                    bytes_copied as f64 / elapsed.as_secs_f64() / 1_000_000.0
                );
            }

            return Ok(0);
        }

        // Not BGZF, need to reopen for transcoding
        drop(file);
    }

    // Get total file size for progress display (if not stdin)
    let total_size =
        if !is_stdin { std::fs::metadata(&args.input).ok().map(|m| m.len()) } else { None };

    // Set up progress tracking if enabled
    let progress_state = if args.progress {
        Some(Arc::new(ProgressState {
            bytes_read: AtomicU64::new(0),
            total_size,
            done: AtomicBool::new(false),
        }))
    } else {
        None
    };

    // Spawn progress thread if enabled
    let progress_handle =
        progress_state.as_ref().map(|state| spawn_progress_thread(Arc::clone(state)));

    // Open input for transcoding (with optional progress wrapper)
    let input: Box<dyn Read> = if is_stdin {
        if let Some(ref state) = progress_state {
            Box::new(ProgressReader::new(io::stdin().lock(), Arc::clone(state)))
        } else {
            Box::new(io::stdin().lock())
        }
    } else {
        let file = BufReader::new(File::open(&args.input)?);
        if let Some(ref state) = progress_state {
            Box::new(ProgressReader::new(file, Arc::clone(state)))
        } else {
            Box::new(file)
        }
    };

    // Open output
    let output: Box<dyn io::Write> = if is_stdout {
        Box::new(io::stdout().lock())
    } else {
        Box::new(BufWriter::new(File::create(output_path)?))
    };

    // Run transcoder
    let start = std::time::Instant::now();

    let stats = if config.num_threads == 1 {
        let mut transcoder = SingleThreadedTranscoder::new(config);
        transcoder.transcode(input, output)?
    } else {
        let mut transcoder = ParallelTranscoder::new(config);
        transcoder.transcode(input, output)?
    };

    let elapsed = start.elapsed();

    // Signal progress thread to stop and wait for it
    if let Some(ref state) = progress_state {
        state.done.store(true, Ordering::Relaxed);
    }
    if let Some(handle) = progress_handle {
        let _ = handle.join();
    }

    // Write index file if requested
    if let (Some(path), Some(entries)) = (&index_path, &stats.index_entries) {
        let mut index_file = BufWriter::new(File::create(path)?);
        // Write number of entries (u64 LE)
        index_file.write_all(&(entries.len() as u64).to_le_bytes())?;
        // Write each entry (compressed_offset, uncompressed_offset as u64 LE pairs)
        for entry in entries {
            index_file.write_all(&entry.compressed_offset.to_le_bytes())?;
            index_file.write_all(&entry.uncompressed_offset.to_le_bytes())?;
        }
        index_file.flush()?;

        if args.verbose {
            eprintln!("Index written: {} ({} entries)", path.display(), entries.len());
        }
    }

    if !args.quiet && (args.verbose || args.progress) {
        eprintln!("Transcoding complete:");
        eprintln!("  Input bytes:      {}", stats.input_bytes);
        eprintln!("  Output bytes:     {}", stats.output_bytes);
        eprintln!("  BGZF blocks:      {}", stats.blocks_written);
        eprintln!("  Boundary refs:    {}", stats.boundary_refs_resolved);
        eprintln!("  Time:             {:.2?}", elapsed);
        eprintln!(
            "  Throughput:       {:.1} MB/s",
            stats.input_bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0
        );
    }

    Ok(0)
}

fn run_check_mode(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let is_stdin = args.input.to_str() == Some("-");

    let validation = if is_stdin {
        let mut stdin = io::stdin().lock();
        if args.strict {
            // Use streaming validation for stdin (no seek required)
            validate_bgzf_streaming(&mut stdin)?
        } else {
            BgzfValidation {
                is_valid_bgzf: is_bgzf(&mut stdin)?,
                block_count: None,
                total_uncompressed_size: None,
            }
        }
    } else {
        let mut file = BufReader::new(File::open(&args.input)?);

        if args.strict {
            validate_bgzf_strict(&mut file)?
        } else {
            BgzfValidation {
                is_valid_bgzf: is_bgzf(&mut file)?,
                block_count: None,
                total_uncompressed_size: None,
            }
        }
    };

    // Output results
    if args.json {
        println!(
            "{{\"is_bgzf\":{},\"block_count\":{},\"uncompressed_size\":{}}}",
            validation.is_valid_bgzf,
            validation.block_count.map(|b| b.to_string()).unwrap_or_else(|| "null".to_string()),
            validation
                .total_uncompressed_size
                .map(|s| s.to_string())
                .unwrap_or_else(|| "null".to_string())
        );
    } else if !args.quiet {
        eprintln!("BGZF: {}", if validation.is_valid_bgzf { "yes" } else { "no" });

        if let Some(blocks) = validation.block_count {
            eprintln!("Blocks: {}", blocks);
        }
        if let Some(size) = validation.total_uncompressed_size {
            eprintln!("Uncompressed size: {} bytes", size);
        }
    }

    if validation.is_valid_bgzf {
        Ok(EXIT_IS_BGZF)
    } else {
        Ok(EXIT_NOT_BGZF)
    }
}

fn run_verify_mode(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let is_stdin = args.input.to_str() == Some("-");

    // Get file size for progress (if not stdin)
    let total_size =
        if !is_stdin { std::fs::metadata(&args.input).ok().map(|m| m.len()) } else { None };

    // Set up progress tracking if enabled
    let progress_state = if args.progress {
        Some(Arc::new(ProgressState {
            bytes_read: AtomicU64::new(0),
            total_size,
            done: AtomicBool::new(false),
        }))
    } else {
        None
    };

    // Spawn progress thread if enabled
    let progress_handle =
        progress_state.as_ref().map(|state| spawn_progress_thread(Arc::clone(state)));

    let start = Instant::now();

    let verification: BgzfVerification = if is_stdin {
        let stdin = io::stdin().lock();
        if let Some(ref state) = progress_state {
            verify_bgzf(&mut ProgressReader::new(stdin, Arc::clone(state)))?
        } else {
            verify_bgzf(&mut io::stdin().lock())?
        }
    } else {
        let file = BufReader::new(File::open(&args.input)?);
        if let Some(ref state) = progress_state {
            verify_bgzf(&mut ProgressReader::new(file, Arc::clone(state)))?
        } else {
            verify_bgzf(&mut BufReader::new(File::open(&args.input)?))?
        }
    };

    let elapsed = start.elapsed();

    // Signal progress thread to stop
    if let Some(ref state) = progress_state {
        state.done.store(true, Ordering::Relaxed);
    }
    if let Some(handle) = progress_handle {
        let _ = handle.join();
    }

    // Determine overall validity
    let is_valid = verification.is_valid_bgzf && verification.crc_valid && verification.isize_valid;

    // Output results
    if args.json {
        println!(
            "{{\"valid\":{},\"is_valid_bgzf\":{},\"crc_valid\":{},\"isize_valid\":{},\"block_count\":{},\"compressed_size\":{},\"uncompressed_size\":{},\"first_error_block\":{},\"first_error\":{}}}",
            is_valid,
            verification.is_valid_bgzf,
            verification.crc_valid,
            verification.isize_valid,
            verification.block_count,
            verification.compressed_size,
            verification.uncompressed_size,
            verification.first_error_block.map(|b| b.to_string()).unwrap_or_else(|| "null".to_string()),
            verification.first_error.as_ref().map(|e| format!("\"{}\"", e.replace('\"', "\\\""))).unwrap_or_else(|| "null".to_string())
        );
    } else if !args.quiet {
        eprintln!("Valid: {}", if is_valid { "yes" } else { "no" });
        eprintln!("BGZF structure: {}", if verification.is_valid_bgzf { "ok" } else { "invalid" });
        eprintln!("CRC32 checksums: {}", if verification.crc_valid { "ok" } else { "MISMATCH" });
        eprintln!("ISIZE values: {}", if verification.isize_valid { "ok" } else { "MISMATCH" });
        eprintln!("Blocks: {}", verification.block_count);
        eprintln!("Compressed size: {} bytes", verification.compressed_size);
        eprintln!("Uncompressed size: {} bytes", verification.uncompressed_size);

        if let Some(err) = &verification.first_error {
            if let Some(block) = verification.first_error_block {
                eprintln!("First error at block {}: {}", block, err);
            } else {
                eprintln!("Error: {}", err);
            }
        }

        if args.verbose || args.progress {
            let throughput = if elapsed.as_secs_f64() > 0.0 {
                verification.compressed_size as f64 / elapsed.as_secs_f64() / 1_000_000.0
            } else {
                0.0
            };
            eprintln!("Time: {:.2?}", elapsed);
            eprintln!("Throughput: {:.1} MB/s", throughput);
        }
    }

    if is_valid {
        Ok(EXIT_VERIFY_VALID)
    } else {
        Ok(EXIT_VERIFY_INVALID)
    }
}

fn run_stats_mode(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let is_stdin = args.input.to_str() == Some("-");

    // Get file size
    let file_size =
        if !is_stdin { std::fs::metadata(&args.input).ok().map(|m| m.len()) } else { None };

    // First, check if it's BGZF
    let is_bgzf_file = if is_stdin {
        // For stdin, we need to read the data and check
        // Use streaming validation which will tell us format
        let mut stdin = io::stdin().lock();
        let validation = validate_bgzf_streaming(&mut stdin)?;
        validation.is_valid_bgzf
    } else {
        let mut file = BufReader::new(File::open(&args.input)?);
        is_bgzf(&mut file)?
    };

    // For BGZF files, get detailed statistics
    let validation = if is_bgzf_file && !is_stdin {
        let mut file = BufReader::new(File::open(&args.input)?);
        Some(validate_bgzf_strict(&mut file)?)
    } else {
        None
    };

    if args.json {
        // JSON output
        let block_count = validation.as_ref().and_then(|v| v.block_count);
        let uncompressed_size = validation.as_ref().and_then(|v| v.total_uncompressed_size);
        let ratio = match (file_size, uncompressed_size) {
            (Some(f), Some(u)) if u > 0 => Some(u as f64 / f as f64),
            _ => None,
        };

        println!(
            "{{\"file\":\"{}\",\"file_size\":{},\"format\":\"{}\",\"block_count\":{},\"uncompressed_size\":{},\"compression_ratio\":{}}}",
            args.input.display().to_string().replace('\"', "\\\""),
            file_size.map(|s| s.to_string()).unwrap_or_else(|| "null".to_string()),
            if is_bgzf_file { "bgzf" } else { "gzip" },
            block_count.map(|b| b.to_string()).unwrap_or_else(|| "null".to_string()),
            uncompressed_size.map(|s| s.to_string()).unwrap_or_else(|| "null".to_string()),
            ratio.map(|r| format!("{:.2}", r)).unwrap_or_else(|| "null".to_string())
        );
    } else if !args.quiet {
        eprintln!("File: {}", args.input.display());
        if let Some(size) = file_size {
            eprintln!("File size: {} bytes ({})", size, format_bytes(size));
        }
        eprintln!("Format: {}", if is_bgzf_file { "BGZF" } else { "gzip" });

        if let Some(validation) = validation {
            if let Some(blocks) = validation.block_count {
                eprintln!("BGZF blocks: {}", blocks);
                if blocks > 1 {
                    // EOF block is typically 28 bytes
                    let data_blocks = blocks - 1;
                    eprintln!("Data blocks: {}", data_blocks);
                }
            }

            if let Some(uncompressed) = validation.total_uncompressed_size {
                eprintln!(
                    "Uncompressed size: {} bytes ({})",
                    uncompressed,
                    format_bytes(uncompressed)
                );

                if let Some(size) = file_size {
                    let ratio = uncompressed as f64 / size as f64;
                    let compression_pct = (1.0 - (size as f64 / uncompressed as f64)) * 100.0;
                    eprintln!("Compression ratio: {:.2}x", ratio);
                    eprintln!("Space savings: {:.1}%", compression_pct);

                    if let Some(blocks) = validation.block_count {
                        if blocks > 1 {
                            let avg_compressed = (size - 28) as f64 / (blocks - 1) as f64;
                            let avg_uncompressed = uncompressed as f64 / (blocks - 1) as f64;
                            eprintln!("Avg compressed block: {:.0} bytes", avg_compressed);
                            eprintln!("Avg uncompressed block: {:.0} bytes", avg_uncompressed);
                        }
                    }
                }
            }
        } else if !is_bgzf_file && !is_stdin {
            // For plain gzip, try to decompress and get size
            eprintln!("Note: For detailed gzip statistics, use --verify mode");
        }
    }

    Ok(0)
}
