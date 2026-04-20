use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cti::{
    save_raster, section_type_name, CompressionType, CTIDecoder, CTIEncoder, CTIConfig,
    SEC_TYPE_ICC, SEC_TYPE_PYLV, SEC_TYPE_RES, SEC_TYPE_TMOD,
};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "cti", version, about = "CTI (Custom Tiled Image) tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Encode image -> CTI
    Encode {
        input: PathBuf,
        output: PathBuf,
        /// NDK preset: tile=4096, Adaptive, RCT off (lossless)
        #[arg(long)]
        ndk: bool,
        /// Named preset profile
        #[arg(long, value_enum)]
        preset: Option<PresetArg>,
        /// Force reversible color decorrelation for RGB (RCT for RGB8, DeltaG for RGB16)
        #[arg(long)]
        rct: bool,
        /// Compression override
        #[arg(long, value_enum)]
        compression: Option<CompressionArg>,
        /// Zstd level (1..=15), default 6
        #[arg(long, default_value_t = 6)]
        zstd_level: i32,
        /// Tile size (default 4096 with --ndk, else 256)
        #[arg(long)]
        tile: Option<u32>,
        /// Build embedded pyramid levels (each level downsampled by 2x)
        #[arg(long, default_value_t = 0)]
        pyramid_levels: u32,
    },

    /// Decode CTI -> raw (and optional image file by extension)
    Decode {
        input: PathBuf,
        raw_out: PathBuf,
        /// Optional image output path (.png, .tif, ...)
        #[arg(long)]
        image_out: Option<PathBuf>,
        /// Backward-compatible PNG output alias
        #[arg(long)]
        png_out: Option<PathBuf>,
        /// Decode pyramid level (0 = full resolution)
        #[arg(long, default_value_t = 0)]
        level: u32,
    },

    /// Decode one tile only
    DecodeTile {
        input: PathBuf,
        tx: u32,
        ty: u32,
        raw_out: PathBuf,
        /// Optional image output path (.png, .tif, ...)
        #[arg(long)]
        image_out: Option<PathBuf>,
        /// Decode pyramid level (0 = full resolution)
        #[arg(long, default_value_t = 0)]
        level: u32,
    },

    /// Decode a rectangular region only
    ExtractRegion {
        input: PathBuf,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        raw_out: PathBuf,
        /// Optional image output path (.png, .tif, ...)
        #[arg(long)]
        image_out: Option<PathBuf>,
        /// Decode pyramid level (0 = full resolution)
        #[arg(long, default_value_t = 0)]
        level: u32,
    },

    /// Print CTI header and metadata info
    Info {
        input: PathBuf,
    },

    /// Benchmark encode/decode throughput
    Bench {
        #[command(subcommand)]
        what: BenchWhat,
    },

    /// Dump section TOC and metadata payload summary
    DumpSections {
        input: PathBuf,
    },
}

#[derive(Subcommand)]
enum BenchWhat {
    /// Benchmark encoding image -> CTI
    Encode {
        input: PathBuf,
        /// Output CTI file (if omitted, uses <input>.cti)
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        ndk: bool,
        #[arg(long, value_enum)]
        preset: Option<PresetArg>,
        /// Force reversible color decorrelation for RGB (RCT for RGB8, DeltaG for RGB16)
        #[arg(long)]
        rct: bool,
        #[arg(long, value_enum)]
        compression: Option<CompressionArg>,
        #[arg(long, default_value_t = 6)]
        zstd_level: i32,
        #[arg(long)]
        tile: Option<u32>,
        #[arg(long, default_value_t = 0)]
        pyramid_levels: u32,
        /// Repeat N times (default 3)
        #[arg(long, default_value_t = 3)]
        repeat: u32,
    },
    /// Benchmark decoding CTI -> RAW
    Decode {
        input: PathBuf,
        /// Optional raw output path (if omitted, output is discarded)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Decode pyramid level (0 = full resolution)
        #[arg(long, default_value_t = 0)]
        level: u32,
        /// Repeat N times (default 5)
        #[arg(long, default_value_t = 5)]
        repeat: u32,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompressionArg {
    None,
    Rle,
    Lz77,
    Delta,
    Predictive,
    Zstd,
    Lz4,
    Adaptive,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PresetArg {
    Archive,
    Web,
    WebZstd,
}

impl From<CompressionArg> for CompressionType {
    fn from(value: CompressionArg) -> Self {
        match value {
            CompressionArg::None => CompressionType::None,
            CompressionArg::Rle => CompressionType::RLE,
            CompressionArg::Lz77 => CompressionType::LZ77,
            CompressionArg::Delta => CompressionType::Delta,
            CompressionArg::Predictive => CompressionType::Predictive,
            CompressionArg::Zstd => CompressionType::Zstd,
            CompressionArg::Lz4 => CompressionType::Lz4,
            CompressionArg::Adaptive => CompressionType::Adaptive,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Encode {
            input,
            output,
            ndk,
            preset,
            rct,
            compression,
            zstd_level,
            tile,
            pyramid_levels,
        } => {
            let cfg = build_config(ndk, preset, rct, compression, zstd_level, tile, pyramid_levels)?;
            let enc = CTIEncoder::new(cfg.clone());
            let info = enc.inspect_input(&input)?;
            println!("Loaded image: {}x{}, {:?}", info.width, info.height, info.color_type);
            println!(
                "Preset: tile={}, comp={:?}, RCT={}, zstd_level={}, pyramid_levels={}, downcast16to8={}",
                cfg.tile_size,
                cfg.compression,
                cfg.color_transform,
                cfg.zstd_level,
                cfg.pyramid_levels,
                cfg.downcast_16_to_8
            );
            enc.encode_path_to_cti(&input, &output)?;
            println!("Wrote CTI -> {}", output.display());
        }

        Commands::Decode {
            input,
            raw_out,
            image_out,
            png_out,
            level,
        } => {
            let output_image = resolve_image_out(image_out, png_out)?;
            let decoded = CTIDecoder::decode_detailed(&input, level)?;
            println!(
                "Decoded CTI: {}x{}, ct={}, comp={}, tile={}, level={}",
                decoded.header.width,
                decoded.header.height,
                decoded.header.color_type,
                decoded.header.compression,
                decoded.header.tile_size,
                level
            );

            write_all(&raw_out, &decoded.data)?;
            println!("Raw written -> {}", raw_out.display());

            if let Some(out) = output_image {
                save_raster(
                    &out,
                    decoded.header.width,
                    decoded.header.height,
                    decoded.header.color_type,
                    &decoded.data,
                )?;
                println!("Image written -> {}", out.display());
            }
        }

        Commands::DecodeTile {
            input,
            tx,
            ty,
            raw_out,
            image_out,
            level,
        } => {
            let tile = CTIDecoder::decode_tile(&input, tx, ty, level)?;
            println!(
                "Decoded tile ({}, {}) at level {}: {}x{}, ct={}",
                tx, ty, level, tile.width, tile.height, tile.color_type
            );
            write_all(&raw_out, &tile.data)?;
            println!("Raw written -> {}", raw_out.display());
            if let Some(out) = image_out {
                save_raster(&out, tile.width, tile.height, tile.color_type, &tile.data)?;
                println!("Image written -> {}", out.display());
            }
        }

        Commands::ExtractRegion {
            input,
            x,
            y,
            width,
            height,
            raw_out,
            image_out,
            level,
        } => {
            let region = CTIDecoder::extract_region(&input, x, y, width, height, level)?;
            println!(
                "Decoded region x={}, y={}, w={}, h={} at level {}: ct={}",
                x, y, width, height, level, region.color_type
            );
            write_all(&raw_out, &region.data)?;
            println!("Raw written -> {}", raw_out.display());
            if let Some(out) = image_out {
                save_raster(&out, region.width, region.height, region.color_type, &region.data)?;
                println!("Image written -> {}", out.display());
            }
        }

        Commands::Info { input } => {
            let info = CTIDecoder::info(&input)?;
            println!("CTI v{}", info.header.version);
            println!("Size: {} x {}", info.header.width, info.header.height);
            println!(
                "Tile: {} ({} x {} tiles)",
                info.header.tile_size, info.header.tiles_x, info.header.tiles_y
            );
            println!("ColorType ID: {}", info.header.color_type);
            println!(
                "Compression: {} ({})",
                info.header.compression,
                CompressionType::from_id(info.header.compression)?.label()
            );
            println!("Quality: {}", info.header.quality);
            println!("Flags: 0x{:04X} (RCT:{})", info.header.flags, (info.header.flags & 1) != 0);
            println!("Sections: {}", info.sections.len());
            if let (Some(xdpi), Some(ydpi)) = (info.xdpi, info.ydpi) {
                println!("DPI: {:.2} x {:.2}", xdpi, ydpi);
            }
            if let Some(icc_size) = info.icc_size {
                println!("ICC profile: {} bytes", icc_size);
            }
            if info.pyramid_levels.is_empty() {
                println!("Pyramid: none");
            } else {
                println!("Pyramid levels: {}", info.pyramid_levels.len());
                for lvl in info.pyramid_levels {
                    println!(
                        "  L{}: {}x{}, tile={}, ct={}, comp={}, payload={} B",
                        lvl.level,
                        lvl.width,
                        lvl.height,
                        lvl.tile_size,
                        lvl.color_type,
                        lvl.compression,
                        lvl.payload_size
                    );
                }
            }
        }

        Commands::Bench { what } => match what {
            BenchWhat::Encode {
                input,
                out,
                ndk,
                preset,
                rct,
                compression,
                zstd_level,
                tile,
                pyramid_levels,
                repeat,
            } => {
                bench_encode(
                    input,
                    out,
                    ndk,
                    preset,
                    rct,
                    compression,
                    zstd_level,
                    tile,
                    pyramid_levels,
                    repeat,
                )?;
            }
            BenchWhat::Decode {
                input,
                out,
                level,
                repeat,
            } => {
                bench_decode(input, out, level, repeat)?;
            }
        },

        Commands::DumpSections { input } => {
            let info = CTIDecoder::info(&input)?;
            let sections = CTIDecoder::sections(&input)?;
            println!("Section count: {}", sections.len());
            let mut pyramid_iter = info.pyramid_levels.iter();
            for (idx, sec) in sections.iter().enumerate() {
                println!(
                    "[{}] type=0x{:08X} '{}' size={} B",
                    idx,
                    sec.ty,
                    section_type_name(sec.ty),
                    sec.payload.len()
                );
                match sec.ty {
                    SEC_TYPE_RES => {
                        if sec.payload.len() == 8 {
                            let x = f32::from_le_bytes(sec.payload[0..4].try_into().unwrap());
                            let y = f32::from_le_bytes(sec.payload[4..8].try_into().unwrap());
                            println!("    DPI: {:.2} x {:.2}", x, y);
                        }
                    }
                    SEC_TYPE_ICC => {
                        println!("    ICC bytes: {}", sec.payload.len());
                    }
                    SEC_TYPE_PYLV => {
                        if let Some(level) = pyramid_iter.next() {
                            println!(
                                "    Pyramid L{}: {}x{}, tile={}, ct={}, comp={}",
                                level.level,
                                level.width,
                                level.height,
                                level.tile_size,
                                level.color_type,
                                level.compression
                            );
                        }
                    }
                    SEC_TYPE_TMOD => {
                        println!("    Tile modes: {}", sec.payload.len());
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn resolve_image_out(image_out: Option<PathBuf>, png_out: Option<PathBuf>) -> Result<Option<PathBuf>> {
    match (image_out, png_out) {
        (Some(_), Some(_)) => bail!("Use either --image-out or --png-out, not both"),
        (Some(path), None) => Ok(Some(path)),
        (None, Some(path)) => Ok(Some(path)),
        (None, None) => Ok(None),
    }
}

fn write_all(path: &PathBuf, data: &[u8]) -> Result<()> {
    let mut bw = BufWriter::new(File::create(path)?);
    bw.write_all(data)?;
    bw.flush()?;
    Ok(())
}

fn build_config(
    ndk: bool,
    preset: Option<PresetArg>,
    rct: bool,
    compression: Option<CompressionArg>,
    zstd_level: i32,
    tile: Option<u32>,
    pyramid_levels: u32,
) -> Result<CTIConfig> {
    if ndk && preset.is_some() {
        bail!("Use either --ndk or --preset, not both");
    }

    let mut cfg = match preset {
        Some(PresetArg::Archive) => CTIConfig {
            tile_size: tile.unwrap_or(4096),
            compression: CompressionType::Adaptive,
            quality_level: 100,
            color_transform: false,
            zstd_level,
            pyramid_levels,
            downcast_16_to_8: false,
        },
        Some(PresetArg::Web) => CTIConfig {
            tile_size: tile.unwrap_or(512),
            compression: CompressionType::Lz4,
            quality_level: 85,
            color_transform: false,
            zstd_level,
            pyramid_levels: if pyramid_levels == 0 { 1 } else { pyramid_levels },
            downcast_16_to_8: true,
        },
        Some(PresetArg::WebZstd) => CTIConfig {
            tile_size: tile.unwrap_or(512),
            compression: CompressionType::Zstd,
            quality_level: 85,
            color_transform: false,
            zstd_level,
            pyramid_levels: if pyramid_levels == 0 { 1 } else { pyramid_levels },
            downcast_16_to_8: true,
        },
        None if ndk => CTIConfig {
            tile_size: tile.unwrap_or(4096),
            compression: CompressionType::Adaptive,
            quality_level: 100,
            color_transform: false,
            zstd_level,
            pyramid_levels,
            downcast_16_to_8: false,
        },
        None => CTIConfig {
            tile_size: tile.unwrap_or(256),
            zstd_level,
            pyramid_levels,
            ..CTIConfig::default()
        },
    };

    if let Some(kind) = compression {
        cfg.compression = kind.into();
    }
    if rct {
        cfg.color_transform = true;
    }
    Ok(cfg)
}

fn bench_encode(
    input_image: PathBuf,
    out_path_opt: Option<PathBuf>,
    ndk: bool,
    preset: Option<PresetArg>,
    rct: bool,
    compression: Option<CompressionArg>,
    zstd_level: i32,
    tile: Option<u32>,
    pyramid_levels: u32,
    repeat: u32,
) -> Result<()> {
    let out_path = out_path_opt.unwrap_or_else(|| input_image.with_extension("cti"));
    let cfg = build_config(ndk, preset, rct, compression, zstd_level, tile, pyramid_levels)?;
    let enc = CTIEncoder::new(cfg.clone());
    let info = enc.inspect_input(&input_image)?;
    println!(
        "BENCH encode: {} ({}x{}, {:?}) -> {} (tile={}, comp={:?}, RCT={}, zstd_level={}, pyramid_levels={}, downcast16to8={})",
        input_image.display(),
        info.width,
        info.height,
        info.color_type,
        out_path.display(),
        cfg.tile_size,
        cfg.compression,
        cfg.color_transform,
        cfg.zstd_level,
        cfg.pyramid_levels,
        cfg.downcast_16_to_8
    );

    let input_bytes = fs::metadata(&input_image)?.len() as f64;
    let px_bpp = match info.color_type {
        image::ColorType::L8 => 1.0,
        image::ColorType::L16 => 2.0,
        image::ColorType::Rgb8 => 3.0,
        image::ColorType::Rgba8 => 4.0,
        image::ColorType::Rgb16 => 6.0,
        _ => bail!("Unsupported color type for bench"),
    };
    let raw_bytes = (info.width as f64) * (info.height as f64) * px_bpp;

    enc.encode_path_to_cti(&input_image, &out_path)?;
    let out_size = fs::metadata(&out_path)?.len() as f64;

    let mut best_ms = f64::INFINITY;
    let mut sum_ms = 0.0;
    for _ in 0..repeat {
        let start = Instant::now();
        enc.encode_path_to_cti(&input_image, &out_path)?;
        let dur = start.elapsed().as_secs_f64() * 1000.0;
        best_ms = best_ms.min(dur);
        sum_ms += dur;
    }
    let avg_ms = sum_ms / (repeat as f64);

    let mb = raw_bytes / (1024.0 * 1024.0);
    let best_mb_s = mb / (best_ms / 1000.0);
    let avg_mb_s = mb / (avg_ms / 1000.0);

    println!("Output size: {:.2} MiB", out_size / (1024.0 * 1024.0));
    println!("Compression ratio vs RAW: {:.3}x", out_size / raw_bytes);
    println!("Compression ratio vs input file: {:.3}x", out_size / input_bytes);
    println!("Time (best/avg over {}): {:.1} ms / {:.1} ms", repeat, best_ms, avg_ms);
    println!(
        "Throughput (best/avg vs RAW): {:.1} MB/s / {:.1} MB/s",
        best_mb_s, avg_mb_s
    );
    Ok(())
}

fn bench_decode(input_cti: PathBuf, out_raw_opt: Option<PathBuf>, level: u32, repeat: u32) -> Result<()> {
    let out_raw = out_raw_opt.unwrap_or_else(|| input_cti.with_extension("raw"));

    let (hdr0, raw0) = CTIDecoder::decode_level(&input_cti, level)?;
    let raw_size = raw0.len() as f64;
    write_all(&out_raw, &raw0)?;
    println!(
        "BENCH decode: {} ({}x{}, ct={}, comp={}, tile={}, level={}) -> {}",
        input_cti.display(),
        hdr0.width,
        hdr0.height,
        hdr0.color_type,
        hdr0.compression,
        hdr0.tile_size,
        level,
        out_raw.display()
    );

    let mut best_ms = f64::INFINITY;
    let mut sum_ms = 0.0;
    for _ in 0..repeat {
        let start = Instant::now();
        let (_hdr, raw) = CTIDecoder::decode_level(&input_cti, level)?;
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
    println!("Time (best/avg over {}): {:.1} ms / {:.1} ms", repeat, best_ms, avg_ms);
    println!(
        "Throughput (best/avg vs RAW): {:.1} MB/s / {:.1} MB/s",
        best_mb_s, avg_mb_s
    );
    Ok(())
}
