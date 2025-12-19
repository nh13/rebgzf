use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use rebgzf::{
    is_bgzf, validate_bgzf_strict, BgzfValidation, ParallelTranscoder, SingleThreadedTranscoder,
    TranscodeConfig, Transcoder,
};

#[derive(Parser, Debug)]
#[command(name = "rebgzf")]
#[command(about = "Convert gzip files to BGZF format efficiently")]
#[command(version)]
struct Args {
    /// Input gzip file (use - for stdin)
    #[arg(short, long)]
    input: PathBuf,

    /// Output BGZF file (use - for stdout)
    #[arg(short, long, required_unless_present = "check")]
    output: Option<PathBuf>,

    /// Number of threads (0 = auto, 1 = single-threaded)
    #[arg(short = 't', long, default_value = "1")]
    threads: usize,

    /// Use fixed Huffman tables (faster but slightly larger output)
    #[arg(long, default_value = "true")]
    fixed_huffman: bool,

    /// BGZF block size (default: 65280)
    #[arg(long, default_value = "65280")]
    block_size: usize,

    /// Show verbose statistics
    #[arg(short, long)]
    verbose: bool,

    /// Check if input is BGZF and exit (0=BGZF, 1=not BGZF, 2=error)
    #[arg(long)]
    check: bool,

    /// Validate all BGZF blocks (slower, more thorough)
    #[arg(long)]
    strict: bool,

    /// Force transcoding even if input is already BGZF
    #[arg(long)]
    force: bool,
}

/// Exit codes for --check mode
const EXIT_IS_BGZF: u8 = 0;
const EXIT_NOT_BGZF: u8 = 1;
const EXIT_ERROR: u8 = 2;

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

    // Normal transcoding mode - output is required
    let output_path = args.output.as_ref().expect("output required when not in check mode");

    let config = TranscodeConfig {
        block_size: args.block_size,
        use_fixed_huffman: args.fixed_huffman,
        num_threads: args.threads,
        strict_bgzf_check: args.strict,
        force_transcode: args.force,
        ..Default::default()
    };

    // Open input
    let is_stdin = args.input.to_str() == Some("-");
    let is_stdout = output_path.to_str() == Some("-");

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

    // Open input for transcoding
    let input: Box<dyn Read> = if is_stdin {
        Box::new(io::stdin().lock())
    } else {
        Box::new(BufReader::new(File::open(&args.input)?))
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

    if args.verbose {
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
        // For stdin, we can only do quick check (can't seek)
        if args.strict {
            eprintln!("Warning: --strict requires seekable input, using quick check for stdin");
        }
        let mut stdin = io::stdin().lock();
        BgzfValidation {
            is_valid_bgzf: is_bgzf(&mut stdin)?,
            block_count: None,
            total_uncompressed_size: None,
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
    eprintln!("BGZF: {}", if validation.is_valid_bgzf { "yes" } else { "no" });

    if let Some(blocks) = validation.block_count {
        eprintln!("Blocks: {}", blocks);
    }
    if let Some(size) = validation.total_uncompressed_size {
        eprintln!("Uncompressed size: {} bytes", size);
    }

    if validation.is_valid_bgzf {
        Ok(EXIT_IS_BGZF)
    } else {
        Ok(EXIT_NOT_BGZF)
    }
}
