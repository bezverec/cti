use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use image::{ColorType, ImageBuffer};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

mod cti;
use cti::{CTIDecoder, CTIEncoder, CTIConfig, CompressionType, TiffImage};

#[derive(Parser)]
#[command(name = "cti", version, about = "CTI (Custom Tiled Image) tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Encode TIFF → CTI
    Encode {
        input: PathBuf,
        output: PathBuf,
        /// NDK preset: tile=4096, Zstd, RCT off (lossless)
        #[arg(long)]
        ndk: bool,
        /// Force RCT (reversible color transform) for RGB (off by default)
        #[arg(long)]
        rct: bool,
        /// Zstd level (1..=15), default 6
        #[arg(long, default_value_t = 6)]
        zstd_level: i32,
        /// Tile size (default 4096 with --ndk, else 256)
        #[arg(long)]
        tile: Option<u32>,
    },

    /// Decode CTI → raw (and optional PNG)
    Decode {
        input: PathBuf,
        raw_out: PathBuf,
        /// Optional PNG path to save preview
        #[arg(long)]
        png_out: Option<PathBuf>,
    },

    /// Print CTI header info
    Info {
        input: PathBuf,
    },

    /// Benchmark encode/decode throughput
    Bench {
        #[command(subcommand)]
        what: BenchWhat,
    },

    /// Dump sections TOC (debug placeholder)
    DumpSections {
        input: PathBuf,
    },
}

#[derive(Subcommand)]
enum BenchWhat {
    /// Benchmark encoding TIFF → CTI
    Encode {
        input: PathBuf,
        /// Output CTI file (if omitted, uses <input>.cti)
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        ndk: bool,
        #[arg(long)]
        rct: bool,
        #[arg(long, default_value_t = 6)]
        zstd_level: i32,
        #[arg(long)]
        tile: Option<u32>,
        /// Repeat N times (default 3)
        #[arg(long, default_value_t = 3)]
        repeat: u32,
    },
    /// Benchmark decoding CTI → RAW
    Decode {
        input: PathBuf,
        /// Optional raw output path (if omitted, output is discarded)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Repeat N times (default 5)
        #[arg(long, default_value_t = 5)]
        repeat: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Encode { input, output, ndk, rct, zstd_level, tile } => {
            let mut cfg = if ndk {
                CTIConfig {
                    tile_size: 4096,
                    compression: CompressionType::Zstd,
                    quality_level: 100,
                    color_transform: false,
                    zstd_level,
                }
            } else {
                CTIConfig {
                    tile_size: tile.unwrap_or(256),
                    zstd_level,
                    ..CTIConfig::default()
                }
            };
            if rct { cfg.color_transform = true; }

            let enc = CTIEncoder::new(cfg.clone());
            let img = enc.load_tiff(&input)?;
            println!("Loaded TIFF: {}x{}, {:?}", img.width, img.height, img.color_type);
            println!(
                "Preset: tile={}, comp={:?}, RCT={}, zstd_level={}",
                cfg.tile_size, cfg.compression, cfg.color_transform, cfg.zstd_level
            );
            enc.encode_to_cti(&img, &output)?;
            println!("Wrote CTI → {}", output.display());
        }

        Commands::Decode { input, raw_out, png_out } => {
            let (hdr, buf) = CTIDecoder::decode(&input)?;
            println!(
                "Decoded CTI: {}x{}, ct={}, comp={}, tile={}",
                hdr.width, hdr.height, hdr.color_type, hdr.compression, hdr.tile_size
            );

            // raw out
            write_all(&raw_out, &buf)?;
            println!("Raw written → {}", raw_out.display());

            if let Some(png) = png_out {
                // try to guess color type from header id
                match hdr.color_type {
                    1 => { // L8
                        let imgbuf: ImageBuffer<image::Luma<u8>, _> =
                            ImageBuffer::from_raw(hdr.width, hdr.height, buf).context("raw->L8")?;
                        imgbuf.save(png.clone())?;
                    }
                    3 => { // RGB8
                        let imgbuf: ImageBuffer<image::Rgb<u8>, _> =
                            ImageBuffer::from_raw(hdr.width, hdr.height, buf).context("raw->RGB8")?;
                        imgbuf.save(png.clone())?;
                    }
                    4 => { // RGBA8
                        let imgbuf: ImageBuffer<image::Rgba<u8>, _> =
                            ImageBuffer::from_raw(hdr.width, hdr.height, buf).context("raw->RGBA8")?;
                        imgbuf.save(png.clone())?;
                    }
                    2 | 5 => {
                        eprintln!("PNG preview for 16-bit types not implemented");
                    }
                    _ => {
                        eprintln!("Unsupported ColorType ID {} for PNG preview", hdr.color_type);
                    }
                }
                println!("PNG written → {}", png.display());
            }
        }

        Commands::Info { input } => {
            let hdr = CTIDecoder::info(&input)?;
            println!("CTI v{}", hdr.version);
            println!("Size: {} x {}", hdr.width, hdr.height);
            println!("Tile: {} ({} x {} tiles)", hdr.tile_size, hdr.tiles_x, hdr.tiles_y);
            println!("ColorType ID: {}", hdr.color_type);
            println!("Compression ID: {}", hdr.compression);
            println!("Quality: {}", hdr.quality);
            println!("Flags: 0x{:04X} (RCT:{})", hdr.flags, (hdr.flags & 1) != 0);
        }

        Commands::Bench { what } => match what {
            BenchWhat::Encode { input, out, ndk, rct, zstd_level, tile, repeat } => {
                bench_encode(input, out, ndk, rct, zstd_level, tile, repeat)?;
            }
            BenchWhat::Decode { input, out, repeat } => {
                bench_decode(input, out, repeat)?;
            }
        },

        Commands::DumpSections { input } => {
            let (hdr, _buf) = CTIDecoder::decode(&input)?;
            println!("CTI sections (placeholder): present after image data.");
            println!(
                "(hdr width={}, height={}, tiles={}x{})",
                hdr.width, hdr.height, hdr.tiles_x, hdr.tiles_y
            );
        }
    }

    Ok(())
}

fn write_all(path: &PathBuf, data: &[u8]) -> Result<()> {
    let mut bw = BufWriter::new(File::create(path)?);
    bw.write_all(data)?;
    bw.flush()?;
    Ok(())
}

fn bench_encode(
    input_tiff: PathBuf,
    out_path_opt: Option<PathBuf>,
    ndk: bool,
    rct: bool,
    zstd_level: i32,
    tile: Option<u32>,
    repeat: u32,
) -> Result<()> {
    let out_path = out_path_opt.unwrap_or_else(|| input_tiff.with_extension("cti"));

    // preset
    let mut cfg = if ndk {
        CTIConfig {
            tile_size: 4096,
            compression: CompressionType::Zstd,
            quality_level: 100,
            color_transform: false,
            zstd_level,
        }
    } else {
        CTIConfig {
            tile_size: tile.unwrap_or(256),
            zstd_level,
            ..CTIConfig::default()
        }
    };
    if rct {
        cfg.color_transform = true;
    }

    let enc = CTIEncoder::new(cfg.clone());
    let img: TiffImage = enc.load_tiff(&input_tiff)?;
    println!(
        "BENCH encode: {} ({}x{}, {:?}) → {} (tile={}, comp={:?}, RCT={}, zstd_level={})",
        input_tiff.display(),
        img.width,
        img.height,
        img.color_type,
        out_path.display(),
        cfg.tile_size,
        cfg.compression,
        cfg.color_transform,
        cfg.zstd_level
    );

    // sizes
    let tiff_bytes = fs::metadata(&input_tiff)?.len() as f64;
    let px_bpp = match img.color_type {
        ColorType::L8 => 1.0,
        ColorType::L16 => 2.0,
        ColorType::Rgb8 => 3.0,
        ColorType::Rgba8 => 4.0,
        ColorType::Rgb16 => 6.0,
        _ => bail!("Unsupported color type for bench"),
    };
    let raw_bytes = (img.width as f64) * (img.height as f64) * px_bpp;

    // warmup
    enc.encode_to_cti(&img, &out_path)?;
    let out_size = fs::metadata(&out_path)?.len() as f64;

    let mut best_ms = f64::INFINITY;
    let mut sum_ms = 0.0;
    for _ in 0..repeat {
        let start = Instant::now();
        enc.encode_to_cti(&img, &out_path)?;
        let dur = start.elapsed().as_secs_f64() * 1000.0;
        best_ms = best_ms.min(dur);
        sum_ms += dur;
    }
    let avg_ms = sum_ms / (repeat as f64);

    // throughput vs RAW size
    let mb = raw_bytes / (1024.0 * 1024.0);
    let best_mb_s = mb / (best_ms / 1000.0);
    let avg_mb_s = mb / (avg_ms / 1000.0);

    println!("Output size: {:.2} MiB", out_size / (1024.0 * 1024.0));
    println!("Compression ratio vs RAW: {:.3}x", out_size / raw_bytes);
    println!("Compression ratio vs TIFF file: {:.3}x", out_size / tiff_bytes);
    println!(
        "Time (best/avg over {}): {:.1} ms / {:.1} ms",
        repeat, best_ms, avg_ms
    );
    println!(
        "Throughput (best/avg vs RAW): {:.1} MB/s / {:.1} MB/s",
        best_mb_s, avg_mb_s
    );
    Ok(())
}

fn bench_decode(input_cti: PathBuf, out_raw_opt: Option<PathBuf>, repeat: u32) -> Result<()> {
    let out_raw = out_raw_opt.unwrap_or_else(|| input_cti.with_extension("raw"));

    // warmup
    let (hdr0, raw0) = CTIDecoder::decode(&input_cti)?;
    let raw_size = raw0.len() as f64;
    write_all(&out_raw, &raw0)?;
    println!(
        "BENCH decode: {} ({}x{}, ct={}, comp={}, tile={}) → {}",
        input_cti.display(),
        hdr0.width,
        hdr0.height,
        hdr0.color_type,
        hdr0.compression,
        hdr0.tile_size,
        out_raw.display()
    );

    let mut best_ms = f64::INFINITY;
    let mut sum_ms = 0.0;
    for _ in 0..repeat {
        let start = Instant::now();
        let (_hdr, raw) = CTIDecoder::decode(&input_cti)?;
        let dur = start.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(&raw);
        best_ms = best_ms.min(dur);
        sum_ms += dur;
    }
    let avg_ms = sum_ms / (repeat as f64);

    let mb = raw_size / (1024.0 * 1024.0);
    let best_mb_s = mb / (best_ms / 1000.0);
    let avg_mb_s = mb / (avg_ms / 1000.0);

    println!("Raw size: {:.2} MiB", mb);
    println!(
        "Time (best/avg over {}): {:.1} ms / {:.1} ms",
        repeat, best_ms, avg_ms
    );
    println!(
        "Throughput (best/avg vs RAW): {:.1} MB/s / {:.1} MB/s",
        best_mb_s, avg_mb_s
    );
    Ok(())
}
