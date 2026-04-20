use anyhow::{anyhow, bail, ensure, Context, Result};
use image::{codecs::tiff::TiffDecoder, ColorType, DynamicImage, GenericImageView, ImageBuffer, ImageDecoder};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{remove_file, File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tiff::decoder::{ChunkType as TiffChunkType, Decoder as RawTiffDecoder, DecodingResult as RawTiffDecodingResult, Limits as TiffLimits};
use tiff::tags::Tag as TiffTag;

pub const CTI_MAGIC: &[u8; 4] = b"CTI1";
pub const CTI_HEADER_SIZE: usize = 64;
pub const TILE_INDEX_ONDISK_SIZE: usize = 8 + 4 + 4 + 4;
pub const DEFAULT_TILE_SIZE: u32 = 256;

const TAG_ESCAPE_FF: u8 = 0x00;
const TAG_RLE: u8 = 0x01;
const TAG_LZ77: u8 = 0x02;

pub const SEC_TYPE_RES: u32 = 0x2053_4552; // 'RES '
pub const SEC_TYPE_ICC: u32 = 0x2043_4349; // 'ICC '
pub const SEC_TYPE_PYLV: u32 = 0x564C_5950; // 'PYLV'
pub const SEC_TYPE_TMOD: u32 = 0x444F_4D54; // 'TMOD'

const FLAG_COLOR_RCT: u16 = 1 << 0;
const FLAG_COLOR_RGB16_DELTA_G: u16 = 1 << 1;

#[derive(Debug, Clone)]
pub struct CTIConfig {
    pub tile_size: u32,
    pub compression: CompressionType,
    pub quality_level: u8,
    pub color_transform: bool,
    pub zstd_level: i32,
    pub pyramid_levels: u32,
    pub downcast_16_to_8: bool,
}

impl Default for CTIConfig {
    fn default() -> Self {
        Self {
            tile_size: DEFAULT_TILE_SIZE,
            compression: CompressionType::Zstd,
            quality_level: 100,
            color_transform: false,
            zstd_level: 6,
            pyramid_levels: 0,
            downcast_16_to_8: false,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None = 0,
    RLE = 1,
    LZ77 = 2,
    Delta = 3,
    Predictive = 4,
    Zstd = 10,
    Lz4 = 11,
    Adaptive = 250,
}

impl CompressionType {
    pub fn from_id(id: u8) -> Result<Self> {
        Ok(match id {
            0 => Self::None,
            1 => Self::RLE,
            2 => Self::LZ77,
            3 => Self::Delta,
            4 => Self::Predictive,
            10 => Self::Zstd,
            11 => Self::Lz4,
            250 => Self::Adaptive,
            _ => bail!("Unknown compression id {}", id),
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RLE => "rle",
            Self::LZ77 => "lz77",
            Self::Delta => "delta",
            Self::Predictive => "predictive",
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Adaptive => "adaptive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AdaptiveTileMode {
    ZstdRaw = 0,
    ZstdDelta16 = 1,
    ZstdPredict16 = 2,
    ZstdShuffle16 = 3,
    ZstdGradient16 = 4,
    Lz4Raw = 5,
}

impl AdaptiveTileMode {
    fn from_byte(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::ZstdRaw,
            1 => Self::ZstdDelta16,
            2 => Self::ZstdPredict16,
            3 => Self::ZstdShuffle16,
            4 => Self::ZstdGradient16,
            5 => Self::Lz4Raw,
            _ => bail!("Unknown adaptive tile mode {}", value),
        })
    }
}

#[derive(Debug, Clone)]
struct TileCompressionResult {
    comp: Vec<u8>,
    adaptive_mode: Option<AdaptiveTileMode>,
}

#[derive(Clone)]
struct CompTile {
    comp: Vec<u8>,
    orig_len: u32,
    crc: u32,
    adaptive_mode: Option<AdaptiveTileMode>,
}

struct PreparedTile {
    tile: Vec<u8>,
    tile_extent: (u32, u32),
}

#[derive(Debug, Clone)]
struct StagedSectionFile {
    ty: u32,
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorTransformMode {
    None,
    Rct,
    Rgb16DeltaG,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CTIHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub flags: u16,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub color_type: u8,
    pub compression: u8,
    pub quality: u8,
    pub reserved: [u8; 33],
}

impl CTIHeader {
    pub fn new(
        width: u32,
        height: u32,
        tile_size: u32,
        tiles_x: u32,
        tiles_y: u32,
        color_type: u8,
        compression: u8,
        quality: u8,
        flags: u16,
    ) -> Self {
        Self {
            magic: *CTI_MAGIC,
            version: 1,
            flags,
            width,
            height,
            tile_size,
            tiles_x,
            tiles_y,
            color_type,
            compression,
            quality,
            reserved: [0u8; 33],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileIndex {
    pub offset: u64,
    pub compressed_size: u32,
    pub original_size: u32,
    pub crc32: u32,
}

#[derive(Debug, Clone)]
pub struct TiffImage {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub data: Vec<u8>,
    pub xdpi: Option<f32>,
    pub ydpi: Option<f32>,
    pub icc: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct InputImageInfo {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub xdpi: Option<f32>,
    pub ydpi: Option<f32>,
    pub icc_size: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct CTISection {
    pub ty: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionDesc {
    pub ty: u32,
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct PyramidLevelInfo {
    pub level: u32,
    pub width: u32,
    pub height: u32,
    pub tile_size: u32,
    pub color_type: u8,
    pub compression: u8,
    pub payload_size: usize,
}

#[derive(Debug, Clone)]
pub struct CTIFileInfo {
    pub header: CTIHeader,
    pub indices: Vec<TileIndex>,
    pub sections: Vec<SectionDesc>,
    pub xdpi: Option<f32>,
    pub ydpi: Option<f32>,
    pub icc_size: Option<usize>,
    pub pyramid_levels: Vec<PyramidLevelInfo>,
}

#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub header: CTIHeader,
    pub data: Vec<u8>,
    pub sections: Vec<CTISection>,
}

#[derive(Debug, Clone)]
pub struct RasterRegion {
    pub width: u32,
    pub height: u32,
    pub color_type: u8,
    pub data: Vec<u8>,
}

pub struct CTIEncoder {
    config: CTIConfig,
}

impl CTIEncoder {
    pub fn new(config: CTIConfig) -> Self {
        Self { config }
    }

    fn pyramid_tile_size(&self) -> u32 {
        self.config.tile_size.min(1024)
    }

    fn prepare_image_for_storage(&self, img: &TiffImage) -> Result<TiffImage> {
        let stored_color_type = storage_color_type(img.color_type, self.config.downcast_16_to_8);
        if stored_color_type == img.color_type {
            return Ok(img.clone());
        }

        Ok(TiffImage {
            width: img.width,
            height: img.height,
            color_type: stored_color_type,
            data: convert_tile_for_storage(&img.data, img.color_type, stored_color_type)?,
            xdpi: img.xdpi,
            ydpi: img.ydpi,
            icc: img.icc.clone(),
        })
    }

    pub fn load_image<P: AsRef<Path>>(&self, path: P) -> Result<TiffImage> {
        let p = path.as_ref();
        let file = File::open(p).with_context(|| format!("open {:?}", p))?;
        let mut br = BufReader::new(file);

        let (width, height, color_type, data, icc_from_decoder) = match TiffDecoder::new(&mut br) {
            Ok(mut d) => {
                let (w, h) = d.dimensions();
                let ct = d.color_type();
                let icc = d.icc_profile().ok().flatten();
                let mut buf = vec![0u8; d.total_bytes() as usize];
                d.read_image(&mut buf)?;
                (w, h, ct, buf, icc)
            }
            Err(_) => {
                let dynimg =
                    image::open(p).with_context(|| format!("image::open fallback for {:?}", p))?;
                let (w, h) = dynimg.dimensions();
                let (ct, buf) = dynamic_image_into_raw(dynimg);
                (w, h, ct, buf, None)
            }
        };

        let (xdpi, ydpi, icc_from_tags) =
            read_tiff_metadata_for_sections(p).unwrap_or((None, None, None));

        Ok(TiffImage {
            width,
            height,
            color_type,
            data,
            xdpi,
            ydpi,
            icc: icc_from_decoder.or(icc_from_tags),
        })
    }

    pub fn load_tiff<P: AsRef<Path>>(&self, path: P) -> Result<TiffImage> {
        self.load_image(path)
    }

    pub fn inspect_input<P: AsRef<Path>>(&self, path: P) -> Result<InputImageInfo> {
        let p = path.as_ref();
        if is_tiff_path(p) {
            return inspect_tiff_input(p);
        }

        let dynimg = image::open(p).with_context(|| format!("image::open for {:?}", p))?;
        let (w, h) = dynimg.dimensions();
        let (ct, _) = dynamic_image_into_raw(dynimg);
        Ok(InputImageInfo {
            width: w,
            height: h,
            color_type: ct,
            xdpi: None,
            ydpi: None,
            icc_size: None,
        })
    }

    pub fn encode_path_to_cti<P: AsRef<Path>, Q: AsRef<Path>>(&self, input_path: P, out_path: Q) -> Result<()> {
        if is_tiff_path(input_path.as_ref()) {
            match self.encode_tiff_streaming(input_path.as_ref(), out_path.as_ref()) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if self.config.pyramid_levels > 0
                        || self.config.tile_size >= 1024
                        || self.config.downcast_16_to_8
                    {
                        return Err(err).with_context(|| {
                            format!(
                                "streaming TIFF encode failed for {:?}; fallback to full image load disabled for this configuration",
                                input_path.as_ref()
                            )
                        });
                    }
                    let img = self.load_image(input_path.as_ref())?;
                    self.encode_to_cti(&img, out_path.as_ref())
                        .with_context(|| format!("streaming TIFF path failed first: {err}"))
                }
            }
        } else {
            let img = self.load_image(input_path.as_ref())?;
            self.encode_to_cti(&img, out_path.as_ref())
        }
    }

    pub fn encode_to_cti<P: AsRef<Path>>(&self, img: &TiffImage, out_path: P) -> Result<()> {
        let mut bw = BufWriter::new(File::create(out_path.as_ref())?);
        self.encode_to_writer(img, &mut bw)?;
        bw.flush()?;
        Ok(())
    }

    pub fn encode_to_writer<W: Write + Seek>(&self, img: &TiffImage, w: &mut W) -> Result<()> {
        let prepared = self.prepare_image_for_storage(img)?;
        let mut sections = build_metadata_sections(&prepared);
        sections.extend(self.build_pyramid_sections(&prepared)?);
        self.encode_image_with_sections(&prepared, w, &sections)
    }

    fn build_pyramid_sections(&self, img: &TiffImage) -> Result<Vec<(u32, Vec<u8>)>> {
        if self.config.pyramid_levels == 0 {
            return Ok(Vec::new());
        }

        let mut current = img.clone();
        let mut out = Vec::new();
        for _ in 0..self.config.pyramid_levels {
            if current.width <= 1 && current.height <= 1 {
                break;
            }
            current = downsample_half(&current)?;
            let cfg = CTIConfig {
                tile_size: self.pyramid_tile_size(),
                pyramid_levels: 0,
                ..self.config.clone()
            };
            let enc = CTIEncoder::new(cfg);
            let mut cursor = Cursor::new(Vec::new());
            enc.encode_to_writer(&current, &mut cursor)?;
            out.push((SEC_TYPE_PYLV, cursor.into_inner()));
        }
        Ok(out)
    }

    fn encode_tiff_streaming(&self, input_path: &Path, out_path: &Path) -> Result<()> {
        let file = File::open(input_path).with_context(|| format!("open {:?}", input_path))?;
        let reader = BufReader::new(file);
        let mut dec = RawTiffDecoder::new(reader)?.with_limits(TiffLimits::unlimited());

        let (width, height) = dec.dimensions()?;
        let color_type = tiff_color_type_to_image_color_type(dec.colortype()?)?;
        let stored_color_type = storage_color_type(color_type, self.config.downcast_16_to_8);
        let bpp = bytes_per_pixel(&color_type)?;
        let chunky = dec
            .find_tag_unsigned::<u16>(TiffTag::PlanarConfiguration)?
            .unwrap_or(1);
        ensure!(chunky == 1, "Only chunky TIFF input is supported for streaming encode");

        let chunk_type = dec.get_chunk_type();
        let (chunk_w_default, chunk_h_default) = dec.chunk_dimensions();
        ensure!(chunk_w_default > 0 && chunk_h_default > 0, "TIFF chunk dimensions must be non-zero");

        let (xdpi, ydpi, icc) =
            read_tiff_metadata_for_sections(input_path).unwrap_or((None, None, None));
        let meta = TiffImage {
            width,
            height,
            color_type: stored_color_type,
            data: Vec::new(),
            xdpi,
            ydpi,
            icc,
        };

        let mut bw = BufWriter::new(File::create(out_path)?);

        let tiles_x = width.div_ceil(self.config.tile_size);
        let tiles_y = height.div_ceil(self.config.tile_size);
        let total_tiles = (tiles_x * tiles_y) as usize;
        let color_type_id = color_type_to_id(stored_color_type)?;

        let color_transform = color_transform_mode_for_encode(self.config.color_transform, stored_color_type);
        let flags = color_transform_flags(color_transform);

        let header = CTIHeader::new(
            width,
            height,
            self.config.tile_size,
            tiles_x,
            tiles_y,
            color_type_id,
            self.config.compression as u8,
            self.config.quality_level,
            flags,
        );
        write_header(&mut bw, &header)?;

        let index_offset = CTI_HEADER_SIZE as u64;
        let index_size = total_tiles * TILE_INDEX_ONDISK_SIZE;
        let data_offset = index_offset + index_size as u64;
        bw.seek(SeekFrom::Start(data_offset))?;

        let mut indices = Vec::with_capacity(total_tiles);
        let mut cursor = data_offset;
        let mut adaptive_modes = Vec::with_capacity(total_tiles);
        let batch_tiles = streaming_batch_tiles(self.config.tile_size, bpp);
        let mut next_idx = 0usize;

        while next_idx < total_tiles {
            let batch_end = (next_idx + batch_tiles).min(total_tiles);
            let mut prepared_tiles = Vec::with_capacity(batch_end - next_idx);

            for idx in next_idx..batch_end {
                let tx = (idx as u32) % tiles_x;
                let ty = (idx as u32) / tiles_x;
                let mut tile = extract_tiff_tile(
                    &mut dec,
                    width,
                    height,
                    color_type,
                    bpp,
                    self.config.tile_size,
                    tx,
                    ty,
                    chunk_type,
                    chunk_w_default,
                    chunk_h_default,
                )?;
                tile = convert_tile_for_storage(&tile, color_type, stored_color_type)?;
                apply_color_transform_forward(color_transform, stored_color_type, &mut tile);
                prepared_tiles.push(PreparedTile {
                    tile,
                    tile_extent: tile_extent(width, height, self.config.tile_size, tx, ty),
                });
            }

            let comp_tiles: Vec<CompTile> = prepared_tiles
                .into_par_iter()
                .map(|prepared| {
                    compress_prepared_tile(
                        self.config.compression,
                        stored_color_type,
                        prepared.tile,
                        prepared.tile_extent,
                        self.config.zstd_level,
                    )
                })
                .collect::<Result<Vec<_>>>()?;

            for comp in comp_tiles {
                if let Some(mode) = comp.adaptive_mode {
                    adaptive_modes.push(mode as u8);
                }
                bw.write_all(&comp.comp)?;
                indices.push(TileIndex {
                    offset: cursor,
                    compressed_size: comp.comp.len() as u32,
                    original_size: comp.orig_len,
                    crc32: comp.crc,
                });
                cursor += comp.comp.len() as u64;
            }

            next_idx = batch_end;
        }

        bw.seek(SeekFrom::Start(index_offset))?;
        for idx in &indices {
            write_tile_index(&mut bw, idx)?;
        }

        bw.flush()?;
        drop(bw);

        let mut base_sections = build_metadata_sections(&meta);
        if !adaptive_modes.is_empty() {
            base_sections.push((SEC_TYPE_TMOD, adaptive_modes));
        }

        {
            let mut section_writer = OpenOptions::new().read(true).write(true).open(out_path)?;
            section_writer.seek(SeekFrom::Start(cursor))?;
            let end = write_sections_with_staged(&mut section_writer, &base_sections, &[])?;
            section_writer.set_len(end)?;
        }

        let pyramid_sections = self.build_pyramid_sections_streaming_to_files(out_path)?;
        let rewrite_result = {
            let mut section_writer = OpenOptions::new().read(true).write(true).open(out_path)?;
            section_writer.seek(SeekFrom::Start(cursor))?;
            let end = write_sections_with_staged(&mut section_writer, &base_sections, &pyramid_sections)?;
            section_writer.set_len(end)?;
            Ok::<(), anyhow::Error>(())
        };
        for section in &pyramid_sections {
            let _ = remove_file(&section.path);
        }
        rewrite_result?;
        Ok(())
    }

    fn build_pyramid_sections_streaming_to_files(&self, base_cti_path: &Path) -> Result<Vec<StagedSectionFile>> {
        if self.config.pyramid_levels == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let mut source_path = base_cti_path.to_path_buf();
        for level in 0..self.config.pyramid_levels {
            let staged_path = staged_pyramid_path(base_cti_path, level + 1);
            trace_pyramid(format!(
                "pyramid level {} start: source={} staged={}",
                level + 1,
                source_path.display(),
                staged_path.display()
            ));
            let size = {
                let file = File::open(&source_path)?;
                let mut src = BufReader::new(file);
                let mut staged = BufWriter::new(File::create(&staged_path)?);
                let size = self.encode_downsampled_level_from_cti_reader(&mut src, &mut staged)?;
                staged.flush()?;
                size
            };

            if size == 0 {
                let _ = remove_file(&staged_path);
                break;
            }

            trace_pyramid(format!("pyramid level {} done: {} bytes", level + 1, size));
            source_path = staged_path.clone();
            out.push(StagedSectionFile {
                ty: SEC_TYPE_PYLV,
                path: staged_path,
                size,
            });
        }
        Ok(out)
    }

    fn encode_downsampled_level_from_cti_reader<R: Read + Seek, W: Write + Seek>(
        &self,
        source: &mut R,
        mut out: &mut W,
    ) -> Result<u64> {
        let layout = scan_cti_layout(source)?;
        if layout.header.width <= 1 && layout.header.height <= 1 {
            return Ok(0);
        }

        let src_header = layout.header;
        let color_type = color_type_from_id(src_header.color_type)?;
        let dst_width = src_header.width.div_ceil(2);
        let dst_height = src_header.height.div_ceil(2);
        let level_tile_size = self.pyramid_tile_size();
        let tiles_x = dst_width.div_ceil(level_tile_size);
        let tiles_y = dst_height.div_ceil(level_tile_size);
        let total_tiles = (tiles_x * tiles_y) as usize;
        trace_pyramid(format!(
            "encode pyramid level: {}x{} tile={} tiles={}x{}",
            dst_width, dst_height, level_tile_size, tiles_x, tiles_y
        ));

        let color_transform = color_transform_mode_for_encode(self.config.color_transform, color_type);
        let flags = color_transform_flags(color_transform);

        let header = CTIHeader::new(
            dst_width,
            dst_height,
            level_tile_size,
            tiles_x,
            tiles_y,
            src_header.color_type,
            self.config.compression as u8,
            self.config.quality_level,
            flags,
        );

        write_header(&mut out, &header)?;
        let index_offset = CTI_HEADER_SIZE as u64;
        let index_size = total_tiles * TILE_INDEX_ONDISK_SIZE;
        let data_offset = index_offset + index_size as u64;
        out.seek(SeekFrom::Start(data_offset))?;

        let mut indices = Vec::with_capacity(total_tiles);
        let mut cursor = data_offset;
        let mut adaptive_modes = Vec::with_capacity(total_tiles);
        let batch_tiles =
            streaming_batch_tiles(level_tile_size, bytes_per_pixel(&color_type)?);
        let mut next_idx = 0usize;
        let mut source_tile_cache: HashMap<usize, Vec<u8>> = HashMap::new();

        while next_idx < total_tiles {
            let batch_end = (next_idx + batch_tiles).min(total_tiles);
            let mut prepared_tiles = Vec::with_capacity(batch_end - next_idx);
            let batch_ty = (next_idx as u32) / tiles_x;
            let min_keep_source_row = (batch_ty * level_tile_size * 2) / src_header.tile_size;
            source_tile_cache.retain(|idx, _| {
                let source_row = (*idx as u32) / src_header.tiles_x;
                source_row >= min_keep_source_row
            });

            for idx in next_idx..batch_end {
                let tx = (idx as u32) % tiles_x;
                let ty = (idx as u32) / tiles_x;
                if idx == 0 {
                    trace_pyramid(format!("encode pyramid first tile ({}, {})", tx, ty));
                }
                let mut tile = downsample_cti_tile_from_layout(
                    source,
                    &layout,
                    color_type,
                    level_tile_size,
                    tx,
                    ty,
                    &mut source_tile_cache,
                )?;

                apply_color_transform_forward(color_transform, color_type, &mut tile);
                prepared_tiles.push(PreparedTile {
                    tile,
                    tile_extent: tile_extent(dst_width, dst_height, level_tile_size, tx, ty),
                });
            }

            let comp_tiles: Vec<CompTile> = prepared_tiles
                .into_par_iter()
                .map(|prepared| {
                    compress_prepared_tile(
                        self.config.compression,
                        color_type,
                        prepared.tile,
                        prepared.tile_extent,
                        self.config.zstd_level,
                    )
                })
                .collect::<Result<Vec<_>>>()?;

            for comp in comp_tiles {
                if let Some(mode) = comp.adaptive_mode {
                    adaptive_modes.push(mode as u8);
                }
                out.write_all(&comp.comp)?;
                indices.push(TileIndex {
                    offset: cursor,
                    compressed_size: comp.comp.len() as u32,
                    original_size: comp.orig_len,
                    crc32: comp.crc,
                });
                cursor += comp.comp.len() as u64;
            }

            next_idx = batch_end;
        }

        out.seek(SeekFrom::Start(index_offset))?;
        for idx in &indices {
            write_tile_index(&mut out, idx)?;
        }
        out.seek(SeekFrom::Start(cursor))?;
        let mut sections = Vec::new();
        if !adaptive_modes.is_empty() {
            sections.push((SEC_TYPE_TMOD, adaptive_modes));
        }
        write_sections(&mut out, &sections)?;
        Ok(out.stream_position()?)
    }

    fn encode_image_with_sections<W: Write + Seek>(
        &self,
        img: &TiffImage,
        w: &mut W,
        extra_sections: &[(u32, Vec<u8>)],
    ) -> Result<()> {
        let tiles_x = img.width.div_ceil(self.config.tile_size);
        let tiles_y = img.height.div_ceil(self.config.tile_size);
        let total_tiles = (tiles_x * tiles_y) as usize;

        let color_type_id = color_type_to_id(img.color_type)?;
        let color_transform = color_transform_mode_for_encode(self.config.color_transform, img.color_type);
        let flags = color_transform_flags(color_transform);

        let header = CTIHeader::new(
            img.width,
            img.height,
            self.config.tile_size,
            tiles_x,
            tiles_y,
            color_type_id,
            self.config.compression as u8,
            self.config.quality_level,
            flags,
        );
        write_header(w, &header)?;

        let index_offset = CTI_HEADER_SIZE as u64;
        let index_size = total_tiles * TILE_INDEX_ONDISK_SIZE;
        let data_offset = index_offset + index_size as u64;
        w.seek(SeekFrom::Start(data_offset))?;

        let zstd_level = self.config.zstd_level;

        let comp_tiles: Vec<CompTile> = (0..total_tiles)
            .into_par_iter()
            .map(|idx| -> Result<CompTile> {
                let tx = (idx as u32) % tiles_x;
                let ty = (idx as u32) / tiles_x;

                let mut tile = extract_tile(img, tx, ty, self.config.tile_size)?;
                apply_color_transform_forward(color_transform, img.color_type, &mut tile);
                compress_prepared_tile(
                    self.config.compression,
                    img.color_type,
                    tile,
                    tile_extent(img.width, img.height, self.config.tile_size, tx, ty),
                    zstd_level,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        let mut indices: Vec<TileIndex> = Vec::with_capacity(total_tiles);
        let mut cursor = data_offset;
        for ct in &comp_tiles {
            w.write_all(&ct.comp)?;
            indices.push(TileIndex {
                offset: cursor,
                compressed_size: ct.comp.len() as u32,
                original_size: ct.orig_len,
                crc32: ct.crc,
            });
            cursor += ct.comp.len() as u64;
        }

        w.seek(SeekFrom::Start(index_offset))?;
        for idx in &indices {
            write_tile_index(w, idx)?;
        }

        w.seek(SeekFrom::Start(cursor))?;
        let mut sections = Vec::with_capacity(extra_sections.len() + 1);
        if comp_tiles.iter().any(|tile| tile.adaptive_mode.is_some()) {
            sections.push((
                SEC_TYPE_TMOD,
                comp_tiles
                    .iter()
                    .map(|tile| tile.adaptive_mode.expect("adaptive mode missing") as u8)
                    .collect(),
            ));
        }
        sections.extend_from_slice(extra_sections);
        write_sections(w, &sections)?;
        Ok(())
    }
}

fn trace_pyramid(message: impl AsRef<str>) {
    if std::env::var_os("CTI_TRACE_PYRAMID").is_some() {
        eprintln!("{}", message.as_ref());
    }
}

pub struct CTIDecoder;

impl CTIDecoder {
    pub fn info<P: AsRef<Path>>(path: P) -> Result<CTIFileInfo> {
        let mut br = BufReader::new(File::open(path)?);
        Self::info_from_reader(&mut br)
    }

    pub fn info_from_reader<R: Read + Seek>(r: &mut R) -> Result<CTIFileInfo> {
        let layout = scan_cti_layout(r)?;
        let sections = read_sections_from_descs(r, &layout.sections)?;
        let (xdpi, ydpi) = sections
            .iter()
            .find(|sec| sec.ty == SEC_TYPE_RES)
            .and_then(|sec| parse_resolution_section(&sec.payload))
            .map(|(x, y)| (Some(x), Some(y)))
            .unwrap_or((None, None));
        let icc_size = sections
            .iter()
            .find(|sec| sec.ty == SEC_TYPE_ICC)
            .map(|sec| sec.payload.len());
        let pyramid_levels = parse_pyramid_levels(&sections)?;

        Ok(CTIFileInfo {
            header: layout.header,
            indices: layout.indices,
            sections: layout.sections,
            xdpi,
            ydpi,
            icc_size,
            pyramid_levels,
        })
    }

    pub fn sections<P: AsRef<Path>>(path: P) -> Result<Vec<CTISection>> {
        let mut br = BufReader::new(File::open(path)?);
        Self::sections_from_reader(&mut br)
    }

    pub fn sections_from_reader<R: Read + Seek>(r: &mut R) -> Result<Vec<CTISection>> {
        let layout = scan_cti_layout(r)?;
        read_sections_from_descs(r, &layout.sections)
    }

    pub fn decode<P: AsRef<Path>>(path: P) -> Result<(CTIHeader, Vec<u8>)> {
        let decoded = Self::decode_detailed(path, 0)?;
        Ok((decoded.header, decoded.data))
    }

    pub fn decode_level<P: AsRef<Path>>(path: P, level: u32) -> Result<(CTIHeader, Vec<u8>)> {
        let decoded = Self::decode_detailed(path, level)?;
        Ok((decoded.header, decoded.data))
    }

    pub fn decode_detailed<P: AsRef<Path>>(path: P, level: u32) -> Result<DecodedImage> {
        let mut br = BufReader::new(File::open(path)?);
        Self::decode_from_reader(&mut br, level)
    }

    pub fn decode_from_reader<R: Read + Seek>(r: &mut R, level: u32) -> Result<DecodedImage> {
        if level > 0 {
            let bytes = read_pyramid_level_bytes(r, level)?;
            let mut cur = Cursor::new(bytes);
            return Self::decode_from_reader(&mut cur, 0);
        }

        let layout = scan_cti_layout(r)?;
        let data = decode_all_tiles(r, &layout)?;
        let sections = read_sections_from_descs(r, &layout.sections)?;
        Ok(DecodedImage {
            header: layout.header,
            data,
            sections,
        })
    }

    pub fn decode_tile<P: AsRef<Path>>(path: P, tx: u32, ty: u32, level: u32) -> Result<RasterRegion> {
        let mut br = BufReader::new(File::open(path)?);
        Self::decode_tile_from_reader(&mut br, tx, ty, level)
    }

    pub fn decode_tile_from_reader<R: Read + Seek>(
        r: &mut R,
        tx: u32,
        ty: u32,
        level: u32,
    ) -> Result<RasterRegion> {
        if level > 0 {
            let bytes = read_pyramid_level_bytes(r, level)?;
            let mut cur = Cursor::new(bytes);
            return Self::decode_tile_from_reader(&mut cur, tx, ty, 0);
        }

        let layout = scan_cti_layout(r)?;
        ensure!(tx < layout.header.tiles_x, "Tile x {} out of range", tx);
        ensure!(ty < layout.header.tiles_y, "Tile y {} out of range", ty);

        let tile_index = (ty * layout.header.tiles_x + tx) as usize;
        let tile = read_decoded_tile(r, &layout, &layout.indices[tile_index], tile_index)?;
        let (tile_w, tile_h) =
            tile_extent(layout.header.width, layout.header.height, layout.header.tile_size, tx, ty);

        Ok(RasterRegion {
            width: tile_w,
            height: tile_h,
            color_type: layout.header.color_type,
            data: tile,
        })
    }

    pub fn extract_region<P: AsRef<Path>>(
        path: P,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        level: u32,
    ) -> Result<RasterRegion> {
        let mut br = BufReader::new(File::open(path)?);
        Self::extract_region_from_reader(&mut br, x, y, width, height, level)
    }

    pub fn extract_region_from_reader<R: Read + Seek>(
        r: &mut R,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        level: u32,
    ) -> Result<RasterRegion> {
        if level > 0 {
            let bytes = read_pyramid_level_bytes(r, level)?;
            let mut cur = Cursor::new(bytes);
            return Self::extract_region_from_reader(&mut cur, x, y, width, height, 0);
        }

        let layout = scan_cti_layout(r)?;
        ensure!(x < layout.header.width, "Region x {} out of range", x);
        ensure!(y < layout.header.height, "Region y {} out of range", y);
        ensure!(width > 0 && height > 0, "Region size must be positive");
        ensure!(x + width <= layout.header.width, "Region exceeds image width");
        ensure!(y + height <= layout.header.height, "Region exceeds image height");

        let bpp = bytes_per_pixel_from_id(layout.header.color_type)?;
        let mut out = vec![0u8; (width * height * bpp) as usize];
        let ts = layout.header.tile_size;
        let tile_x0 = x / ts;
        let tile_y0 = y / ts;
        let tile_x1 = (x + width - 1) / ts;
        let tile_y1 = (y + height - 1) / ts;

        for ty in tile_y0..=tile_y1 {
            for tx in tile_x0..=tile_x1 {
                let idx = (ty * layout.header.tiles_x + tx) as usize;
                let tile = read_decoded_tile(r, &layout, &layout.indices[idx], idx)?;
                let (tile_w, tile_h) = tile_extent(
                    layout.header.width,
                    layout.header.height,
                    layout.header.tile_size,
                    tx,
                    ty,
                );
                blit_tile_region(
                    &mut out,
                    &tile,
                    width,
                    height,
                    x,
                    y,
                    layout.header.tile_size,
                    tile_w,
                    tile_h,
                    bpp,
                    tx,
                    ty,
                )?;
            }
        }

        Ok(RasterRegion {
            width,
            height,
            color_type: layout.header.color_type,
            data: out,
        })
    }
}

pub fn section_type_name(ty: u32) -> &'static str {
    match ty {
        SEC_TYPE_RES => "RES ",
        SEC_TYPE_ICC => "ICC ",
        SEC_TYPE_PYLV => "PYLV",
        SEC_TYPE_TMOD => "TMOD",
        _ => "????",
    }
}

pub fn save_raster<P: AsRef<Path>>(
    path: P,
    width: u32,
    height: u32,
    color_type: u8,
    data: &[u8],
) -> Result<()> {
    match color_type {
        1 => {
            let img: ImageBuffer<image::Luma<u8>, _> =
                ImageBuffer::from_raw(width, height, data.to_vec()).context("raw->L8")?;
            img.save(path)?;
        }
        2 => {
            let img: ImageBuffer<image::Luma<u16>, _> =
                ImageBuffer::from_raw(width, height, bytes_to_u16_vec(data)?).context("raw->L16")?;
            img.save(path)?;
        }
        3 => {
            let img: ImageBuffer<image::Rgb<u8>, _> =
                ImageBuffer::from_raw(width, height, data.to_vec()).context("raw->RGB8")?;
            img.save(path)?;
        }
        4 => {
            let img: ImageBuffer<image::Rgba<u8>, _> =
                ImageBuffer::from_raw(width, height, data.to_vec()).context("raw->RGBA8")?;
            img.save(path)?;
        }
        5 => {
            let img: ImageBuffer<image::Rgb<u16>, _> =
                ImageBuffer::from_raw(width, height, bytes_to_u16_vec(data)?).context("raw->RGB16")?;
            img.save(path)?;
        }
        _ => bail!("Unsupported ColorType ID {} for image output", color_type),
    }
    Ok(())
}

pub fn write_header<W: Write>(w: &mut W, h: &CTIHeader) -> Result<()> {
    w.write_all(&h.magic)?;
    w.write_all(&h.version.to_le_bytes())?;
    w.write_all(&h.flags.to_le_bytes())?;
    w.write_all(&h.width.to_le_bytes())?;
    w.write_all(&h.height.to_le_bytes())?;
    w.write_all(&h.tile_size.to_le_bytes())?;
    w.write_all(&h.tiles_x.to_le_bytes())?;
    w.write_all(&h.tiles_y.to_le_bytes())?;
    w.write_all(&[h.color_type])?;
    w.write_all(&[h.compression])?;
    w.write_all(&[h.quality])?;
    w.write_all(&h.reserved)?;
    Ok(())
}

pub fn read_header<R: Read>(r: &mut R) -> Result<CTIHeader> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    let version = read_u16_le(r)?;
    let flags = read_u16_le(r)?;
    let width = read_u32_le(r)?;
    let height = read_u32_le(r)?;
    let tile_size = read_u32_le(r)?;
    let tiles_x = read_u32_le(r)?;
    let tiles_y = read_u32_le(r)?;
    let color_type = read_u8(r)?;
    let compression = read_u8(r)?;
    let quality = read_u8(r)?;
    let mut reserved = [0u8; 33];
    r.read_exact(&mut reserved)?;
    Ok(CTIHeader {
        magic,
        version,
        flags,
        width,
        height,
        tile_size,
        tiles_x,
        tiles_y,
        color_type,
        compression,
        quality,
        reserved,
    })
}

fn write_tile_index<W: Write>(w: &mut W, t: &TileIndex) -> Result<()> {
    w.write_all(&t.offset.to_le_bytes())?;
    w.write_all(&t.compressed_size.to_le_bytes())?;
    w.write_all(&t.original_size.to_le_bytes())?;
    w.write_all(&t.crc32.to_le_bytes())?;
    Ok(())
}

pub fn read_indices<R: Read>(r: &mut R, n: usize) -> Result<Vec<TileIndex>> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let offset = read_u64_le(r)?;
        let compressed_size = read_u32_le(r)?;
        let original_size = read_u32_le(r)?;
        let crc32 = read_u32_le(r)?;
        v.push(TileIndex {
            offset,
            compressed_size,
            original_size,
            crc32,
        });
    }
    Ok(v)
}

pub fn write_sections<W: Write + Seek>(w: &mut W, sections: &[(u32, Vec<u8>)]) -> Result<()> {
    let count = sections.len() as u32;
    w.write_all(&count.to_le_bytes())?;
    if sections.is_empty() {
        return Ok(());
    }

    let toc_pos = w.stream_position()?;
    let rec_size = 4 + 8 + 8;
    w.seek(SeekFrom::Current((count as i64) * (rec_size as i64)))?;
    let mut descs: Vec<SectionDesc> = Vec::with_capacity(sections.len());
    for (ty, payload) in sections {
        let off = w.stream_position()?;
        w.write_all(payload)?;
        descs.push(SectionDesc {
            ty: *ty,
            offset: off,
            size: payload.len() as u64,
        });
    }

    let end = w.stream_position()?;
    w.seek(SeekFrom::Start(toc_pos))?;
    for d in &descs {
        w.write_all(&d.ty.to_le_bytes())?;
        w.write_all(&d.offset.to_le_bytes())?;
        w.write_all(&d.size.to_le_bytes())?;
    }
    w.seek(SeekFrom::Start(end))?;
    Ok(())
}

fn write_sections_with_staged<W: Write + Seek>(
    w: &mut W,
    memory_sections: &[(u32, Vec<u8>)],
    staged_sections: &[StagedSectionFile],
) -> Result<u64> {
    let total_count = (memory_sections.len() + staged_sections.len()) as u32;
    w.write_all(&total_count.to_le_bytes())?;
    if total_count == 0 {
        return Ok(w.stream_position()?);
    }

    let toc_pos = w.stream_position()?;
    let rec_size = 4 + 8 + 8;
    w.seek(SeekFrom::Current((total_count as i64) * (rec_size as i64)))?;
    let mut descs = Vec::with_capacity(total_count as usize);

    for (ty, payload) in memory_sections {
        let offset = w.stream_position()?;
        w.write_all(payload)?;
        descs.push(SectionDesc {
            ty: *ty,
            offset,
            size: payload.len() as u64,
        });
    }

    for section in staged_sections {
        let offset = w.stream_position()?;
        let mut staged = BufReader::new(File::open(&section.path)?);
        std::io::copy(&mut staged, w)?;
        descs.push(SectionDesc {
            ty: section.ty,
            offset,
            size: section.size,
        });
    }

    let end = w.stream_position()?;
    w.seek(SeekFrom::Start(toc_pos))?;
    for desc in &descs {
        w.write_all(&desc.ty.to_le_bytes())?;
        w.write_all(&desc.offset.to_le_bytes())?;
        w.write_all(&desc.size.to_le_bytes())?;
    }
    w.seek(SeekFrom::Start(end))?;
    Ok(end)
}

fn staged_pyramid_path(base_cti_path: &Path, level: u32) -> PathBuf {
    let mut path = base_cti_path.to_path_buf();
    let ext = base_cti_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!("{ext}.pylv{level}.tmp"))
        .unwrap_or_else(|| format!("pylv{level}.tmp"));
    path.set_extension(ext);
    path
}

fn build_metadata_sections(img: &TiffImage) -> Vec<(u32, Vec<u8>)> {
    let mut sections = Vec::new();
    if let (Some(x), Some(y)) = (img.xdpi, img.ydpi) {
        let mut res = Vec::with_capacity(8);
        res.extend_from_slice(&x.to_le_bytes());
        res.extend_from_slice(&y.to_le_bytes());
        sections.push((SEC_TYPE_RES, res));
    }
    if let Some(icc) = &img.icc {
        sections.push((SEC_TYPE_ICC, icc.clone()));
    }
    sections
}

fn inspect_tiff_input(path: &Path) -> Result<InputImageInfo> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut dec = RawTiffDecoder::new(reader)?;
    let (width, height) = dec.dimensions()?;
    let color_type = tiff_color_type_to_image_color_type(dec.colortype()?)?;
    let (xdpi, ydpi, icc) = read_tiff_metadata_for_sections(path).unwrap_or((None, None, None));
    Ok(InputImageInfo {
        width,
        height,
        color_type,
        xdpi,
        ydpi,
        icc_size: icc.as_ref().map(Vec::len),
    })
}

fn is_tiff_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            ext == "tif" || ext == "tiff"
        })
        .unwrap_or(false)
}

fn tiff_color_type_to_image_color_type(ct: tiff::ColorType) -> Result<ColorType> {
    Ok(match ct {
        tiff::ColorType::Gray(8) => ColorType::L8,
        tiff::ColorType::Gray(16) => ColorType::L16,
        tiff::ColorType::RGB(8) => ColorType::Rgb8,
        tiff::ColorType::RGBA(8) => ColorType::Rgba8,
        tiff::ColorType::RGB(16) => ColorType::Rgb16,
        other => bail!("Unsupported TIFF color type for streaming: {:?}", other),
    })
}

fn color_type_from_id(id: u8) -> Result<ColorType> {
    Ok(match id {
        1 => ColorType::L8,
        2 => ColorType::L16,
        3 => ColorType::Rgb8,
        4 => ColorType::Rgba8,
        5 => ColorType::Rgb16,
        _ => bail!("Unsupported color type id {}", id),
    })
}

fn extract_tiff_tile<R: Read + Seek>(
    dec: &mut RawTiffDecoder<R>,
    width: u32,
    height: u32,
    color_type: ColorType,
    bpp: u32,
    tile_size: u32,
    tx: u32,
    ty: u32,
    chunk_type: TiffChunkType,
    chunk_w_default: u32,
    chunk_h_default: u32,
) -> Result<Vec<u8>> {
    let start_x = tx * tile_size;
    let start_y = ty * tile_size;
    let end_x = (start_x + tile_size).min(width);
    let end_y = (start_y + tile_size).min(height);
    let tile_w = end_x - start_x;
    let tile_h = end_y - start_y;
    let mut out = vec![0u8; (tile_w * tile_h * bpp) as usize];

    let chunks_across = match chunk_type {
        TiffChunkType::Strip => 1,
        TiffChunkType::Tile => width.div_ceil(chunk_w_default),
    };
    let chunks_down = match chunk_type {
        TiffChunkType::Strip => height.div_ceil(chunk_h_default),
        TiffChunkType::Tile => height.div_ceil(chunk_h_default),
    };

    let chunk_x0 = match chunk_type {
        TiffChunkType::Strip => 0,
        TiffChunkType::Tile => start_x / chunk_w_default,
    };
    let chunk_x1 = match chunk_type {
        TiffChunkType::Strip => 0,
        TiffChunkType::Tile => (end_x - 1) / chunk_w_default,
    };
    let chunk_y0 = start_y / chunk_h_default;
    let chunk_y1 = (end_y - 1) / chunk_h_default;

    for cy in chunk_y0..=chunk_y1 {
        for cx in chunk_x0..=chunk_x1 {
            ensure!(cy < chunks_down, "Source TIFF chunk row out of range");
            ensure!(cx < chunks_across, "Source TIFF chunk column out of range");
            let chunk_index = match chunk_type {
                TiffChunkType::Strip => cy,
                TiffChunkType::Tile => cy * chunks_across + cx,
            };

            let chunk_x = match chunk_type {
                TiffChunkType::Strip => 0,
                TiffChunkType::Tile => cx * chunk_w_default,
            };
            let chunk_y = cy * chunk_h_default;
            let actual_chunk_w = match chunk_type {
                TiffChunkType::Strip => width,
                TiffChunkType::Tile => (chunk_x + chunk_w_default).min(width) - chunk_x,
            };
            let actual_chunk_h = (chunk_y + chunk_h_default).min(height) - chunk_y;

            let mut chunk = dec.read_chunk(chunk_index)?;
            let chunk_bytes = chunk_to_le_bytes(&mut chunk, color_type)?;
            blit_chunk_overlap(
                &mut out,
                tile_w,
                tile_h,
                start_x,
                start_y,
                &chunk_bytes,
                actual_chunk_w,
                actual_chunk_h,
                chunk_x,
                chunk_y,
                bpp,
            )?;
        }
    }

    Ok(out)
}

fn chunk_to_le_bytes(chunk: &mut RawTiffDecodingResult, color_type: ColorType) -> Result<Vec<u8>> {
    Ok(match color_type {
        ColorType::L8 | ColorType::Rgb8 | ColorType::Rgba8 => chunk.as_buffer(0).as_bytes().to_vec(),
        ColorType::L16 | ColorType::Rgb16 => {
            if cfg!(target_endian = "little") {
                chunk.as_buffer(0).as_bytes().to_vec()
            } else {
                match chunk {
                    RawTiffDecodingResult::U16(values) => values.iter().flat_map(|v| v.to_le_bytes()).collect(),
                    _ => bail!("Unexpected TIFF chunk type for 16-bit image"),
                }
            }
        }
        _ => bail!("Unsupported color type for chunk conversion: {:?}", color_type),
    })
}

fn blit_chunk_overlap(
    out: &mut [u8],
    tile_w: u32,
    tile_h: u32,
    tile_x: u32,
    tile_y: u32,
    chunk: &[u8],
    chunk_w: u32,
    chunk_h: u32,
    chunk_x: u32,
    chunk_y: u32,
    bpp: u32,
) -> Result<()> {
    let copy_x0 = tile_x.max(chunk_x);
    let copy_y0 = tile_y.max(chunk_y);
    let copy_x1 = (tile_x + tile_w).min(chunk_x + chunk_w);
    let copy_y1 = (tile_y + tile_h).min(chunk_y + chunk_h);
    if copy_x0 >= copy_x1 || copy_y0 >= copy_y1 {
        return Ok(());
    }

    let copy_w = copy_x1 - copy_x0;
    let copy_h = copy_y1 - copy_y0;
    for row in 0..copy_h {
        let src_x = copy_x0 - chunk_x;
        let src_y = (copy_y0 - chunk_y) + row;
        let dst_x = copy_x0 - tile_x;
        let dst_y = (copy_y0 - tile_y) + row;
        let src_off = ((src_y * chunk_w + src_x) * bpp) as usize;
        let dst_off = ((dst_y * tile_w + dst_x) * bpp) as usize;
        let len = (copy_w * bpp) as usize;
        out[dst_off..dst_off + len].copy_from_slice(&chunk[src_off..src_off + len]);
    }
    Ok(())
}

fn downsample_cti_tile_from_layout<R: Read + Seek>(
    source: &mut R,
    layout: &LayoutInfo,
    color_type: ColorType,
    tile_size: u32,
    tx: u32,
    ty: u32,
    tile_cache: &mut HashMap<usize, Vec<u8>>,
) -> Result<Vec<u8>> {
    let src_header = layout.header;
    let dst_width = src_header.width.div_ceil(2);
    let dst_height = src_header.height.div_ceil(2);
    let start_x = tx * tile_size;
    let start_y = ty * tile_size;
    let end_x = (start_x + tile_size).min(dst_width);
    let end_y = (start_y + tile_size).min(dst_height);
    let dst_tile_w = end_x - start_x;
    let dst_tile_h = end_y - start_y;
    let (channels, sample_bytes) = sample_layout(color_type)?;
    let dst_pixels = (dst_tile_w * dst_tile_h) as usize;
    let mut sums = vec![0u32; dst_pixels * channels];
    let mut counts = vec![0u8; dst_pixels];

    let src_start_x = start_x * 2;
    let src_start_y = start_y * 2;
    let src_end_x = (end_x * 2).min(src_header.width);
    let src_end_y = (end_y * 2).min(src_header.height);
    let src_tile_x0 = src_start_x / src_header.tile_size;
    let src_tile_y0 = src_start_y / src_header.tile_size;
    let src_tile_x1 = (src_end_x.saturating_sub(1)) / src_header.tile_size;
    let src_tile_y1 = (src_end_y.saturating_sub(1)) / src_header.tile_size;

    for sty in src_tile_y0..=src_tile_y1 {
        for stx in src_tile_x0..=src_tile_x1 {
            let idx = (sty * src_header.tiles_x + stx) as usize;
            if !tile_cache.contains_key(&idx) {
                let tile = read_decoded_tile(source, layout, &layout.indices[idx], idx)?;
                tile_cache.insert(idx, tile);
            }
            let tile = tile_cache
                .get(&idx)
                .expect("source tile cache missing freshly inserted tile");
            let (tile_w, tile_h) = tile_extent(
                src_header.width,
                src_header.height,
                src_header.tile_size,
                stx,
                sty,
            );
            let tile_origin_x = stx * src_header.tile_size;
            let tile_origin_y = sty * src_header.tile_size;

            for local_y in 0..tile_h as usize {
                let src_y = tile_origin_y + local_y as u32;
                if src_y < src_start_y || src_y >= src_end_y {
                    continue;
                }
                let dst_y = src_y / 2;
                if dst_y < start_y || dst_y >= end_y {
                    continue;
                }

                for local_x in 0..tile_w as usize {
                    let src_x = tile_origin_x + local_x as u32;
                    if src_x < src_start_x || src_x >= src_end_x {
                        continue;
                    }
                    let dst_x = src_x / 2;
                    if dst_x < start_x || dst_x >= end_x {
                        continue;
                    }

                    let dst_index = ((dst_y - start_y) * dst_tile_w + (dst_x - start_x)) as usize;
                    counts[dst_index] = counts[dst_index].saturating_add(1);
                    let src_base = (local_y * tile_w as usize + local_x) * channels * sample_bytes;
                    let dst_base = dst_index * channels;
                    if sample_bytes == 1 {
                        for ch in 0..channels {
                            sums[dst_base + ch] += tile[src_base + ch] as u32;
                        }
                    } else {
                        for ch in 0..channels {
                            let off = src_base + ch * 2;
                            sums[dst_base + ch] +=
                                u16::from_le_bytes([tile[off], tile[off + 1]]) as u32;
                        }
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity((dst_pixels * channels * sample_bytes) as usize);
    for pixel in 0..dst_pixels {
        let count = counts[pixel].max(1) as u32;
        let base = pixel * channels;
        if sample_bytes == 1 {
            for ch in 0..channels {
                out.push((sums[base + ch] / count) as u8);
            }
        } else {
            for ch in 0..channels {
                out.extend_from_slice(&((sums[base + ch] / count) as u16).to_le_bytes());
            }
        }
    }

    Ok(out)
}

#[allow(dead_code)]
fn downsample_packed_image(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    color_type: ColorType,
) -> Result<Vec<u8>> {
    let dst_w = src_w.div_ceil(2);
    let dst_h = src_h.div_ceil(2);
    let bpp = bytes_per_pixel(&color_type)?;
    let mut out = vec![0u8; (dst_w * dst_h * bpp) as usize];
    let (channels, sample_bytes) = sample_layout(color_type)?;
    for dy in 0..dst_h as usize {
        let sy0 = dy * 2;
        let sy1 = (sy0 + 1).min(src_h.saturating_sub(1) as usize);
        for dx in 0..dst_w as usize {
            let sx0 = dx * 2;
            let sx1 = (sx0 + 1).min(src_w.saturating_sub(1) as usize);
            for ch in 0..channels {
                if sample_bytes == 1 {
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for sy in [sy0, sy1] {
                        for sx in [sx0, sx1] {
                            let base = (sy * src_w as usize + sx) * channels + ch;
                            sum += src[base] as u32;
                            count += 1;
                        }
                    }
                    out[(dy * dst_w as usize + dx) * channels + ch] = (sum / count) as u8;
                } else {
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for sy in [sy0, sy1] {
                        for sx in [sx0, sx1] {
                            let base = ((sy * src_w as usize + sx) * channels + ch) * 2;
                            sum += u16::from_le_bytes([src[base], src[base + 1]]) as u32;
                            count += 1;
                        }
                    }
                    let value = (sum / count) as u16;
                    let dst_off = ((dy * dst_w as usize + dx) * channels + ch) * 2;
                    out[dst_off..dst_off + 2].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct LayoutInfo {
    header: CTIHeader,
    indices: Vec<TileIndex>,
    sections: Vec<SectionDesc>,
    adaptive_tile_modes: Option<Vec<u8>>,
}

fn scan_cti_layout<R: Read + Seek>(r: &mut R) -> Result<LayoutInfo> {
    r.seek(SeekFrom::Start(0))?;
    let header = read_header(r)?;
    ensure!(&header.magic == CTI_MAGIC, "Bad magic");
    let total_tiles = (header.tiles_x * header.tiles_y) as usize;
    let indices = read_indices(r, total_tiles)?;

    let data_start = CTI_HEADER_SIZE as u64 + (total_tiles * TILE_INDEX_ONDISK_SIZE) as u64;
    let data_end = indices
        .iter()
        .map(|idx| idx.offset + idx.compressed_size as u64)
        .max()
        .unwrap_or(data_start);
    let sections = read_section_descs(r, data_end)?;
    let adaptive_tile_modes = read_adaptive_tile_modes(r, header.compression, total_tiles, &sections)?;
    Ok(LayoutInfo {
        header,
        indices,
        sections,
        adaptive_tile_modes,
    })
}

fn read_adaptive_tile_modes<R: Read + Seek>(
    r: &mut R,
    compression: u8,
    total_tiles: usize,
    descs: &[SectionDesc],
) -> Result<Option<Vec<u8>>> {
    if compression != CompressionType::Adaptive as u8 {
        return Ok(None);
    }

    let desc = descs
        .iter()
        .find(|desc| desc.ty == SEC_TYPE_TMOD)
        .ok_or_else(|| anyhow!("Adaptive CTI is missing TMOD section"))?;
    ensure!(
        desc.size as usize == total_tiles,
        "TMOD size mismatch: expected {} bytes, got {}",
        total_tiles,
        desc.size
    );

    r.seek(SeekFrom::Start(desc.offset))?;
    let mut payload = vec![0u8; desc.size as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn read_section_descs<R: Read + Seek>(r: &mut R, start: u64) -> Result<Vec<SectionDesc>> {
    r.seek(SeekFrom::Start(start))?;
    let mut count_buf = [0u8; 4];
    match r.read_exact(&mut count_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    }

    let count = u32::from_le_bytes(count_buf);
    let mut descs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        descs.push(SectionDesc {
            ty: read_u32_le(r)?,
            offset: read_u64_le(r)?,
            size: read_u64_le(r)?,
        });
    }
    Ok(descs)
}

fn read_sections_from_descs<R: Read + Seek>(r: &mut R, descs: &[SectionDesc]) -> Result<Vec<CTISection>> {
    let mut out = Vec::with_capacity(descs.len());
    for desc in descs {
        r.seek(SeekFrom::Start(desc.offset))?;
        let mut payload = vec![0u8; desc.size as usize];
        r.read_exact(&mut payload)?;
        out.push(CTISection {
            ty: desc.ty,
            payload,
        });
    }
    Ok(out)
}

fn parse_pyramid_levels(sections: &[CTISection]) -> Result<Vec<PyramidLevelInfo>> {
    let mut out = Vec::new();
    for (idx, sec) in sections.iter().filter(|sec| sec.ty == SEC_TYPE_PYLV).enumerate() {
        let mut cur = Cursor::new(sec.payload.as_slice());
        let hdr = read_header(&mut cur)?;
        ensure!(&hdr.magic == CTI_MAGIC, "Bad pyramid CTI magic");
        out.push(PyramidLevelInfo {
            level: (idx + 1) as u32,
            width: hdr.width,
            height: hdr.height,
            tile_size: hdr.tile_size,
            color_type: hdr.color_type,
            compression: hdr.compression,
            payload_size: sec.payload.len(),
        });
    }
    Ok(out)
}

fn read_pyramid_level_bytes<R: Read + Seek>(r: &mut R, level: u32) -> Result<Vec<u8>> {
    ensure!(level > 0, "Level must be >= 1");
    let sections = CTIDecoder::sections_from_reader(r)?;
    let mut iter = sections.into_iter().filter(|sec| sec.ty == SEC_TYPE_PYLV);
    let bytes = iter
        .nth((level - 1) as usize)
        .map(|sec| sec.payload)
        .ok_or_else(|| anyhow!("Pyramid level {} not present", level))?;
    Ok(bytes)
}

fn decode_all_tiles<R: Read + Seek>(r: &mut R, layout: &LayoutInfo) -> Result<Vec<u8>> {
    let hdr = &layout.header;
    let bpp = bytes_per_pixel_from_id(hdr.color_type)?;
    let mut out = vec![0u8; (hdr.width * hdr.height * bpp) as usize];
    for (i, t) in layout.indices.iter().enumerate() {
        let tile_bytes = read_decoded_tile(r, layout, t, i)?;
        let tx = (i as u32) % hdr.tiles_x;
        let ty = (i as u32) / hdr.tiles_x;
        blit_tile(
            &mut out,
            &tile_bytes,
            hdr.width,
            hdr.height,
            hdr.tile_size,
            bpp,
            tx,
            ty,
        )?;
    }
    Ok(out)
}

fn read_decoded_tile<R: Read + Seek>(
    r: &mut R,
    layout: &LayoutInfo,
    index: &TileIndex,
    tile_number: usize,
) -> Result<Vec<u8>> {
    let hdr = &layout.header;
    r.seek(SeekFrom::Start(index.offset))?;
    let mut comp = vec![0u8; index.compressed_size as usize];
    r.read_exact(&mut comp)?;

    let adaptive_mode = layout
        .adaptive_tile_modes
        .as_ref()
        .map(|modes| AdaptiveTileMode::from_byte(modes[tile_number]))
        .transpose()?;
    let tile_x = tile_number as u32 % hdr.tiles_x;
    let tile_y = tile_number as u32 / hdr.tiles_x;
    let mut tile_bytes = decompress_tile_with_size(
        hdr.compression,
        &comp,
        index.original_size as usize,
        hdr.color_type,
        tile_extent(hdr.width, hdr.height, hdr.tile_size, tile_x, tile_y),
        adaptive_mode,
    )?;
    ensure!(crc32(&tile_bytes) == index.crc32, "CRC mismatch at tile {}", tile_number);

    let color_transform = color_transform_mode_from_header(hdr.flags, hdr.color_type);
    apply_color_transform_inverse(color_transform, hdr.color_type, &mut tile_bytes);
    Ok(tile_bytes)
}

fn dynamic_image_into_raw(img: DynamicImage) -> (ColorType, Vec<u8>) {
    match img {
        DynamicImage::ImageLuma8(buf) => (ColorType::L8, buf.into_raw()),
        DynamicImage::ImageLuma16(buf) => (
            ColorType::L16,
            buf.into_raw()
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        ),
        DynamicImage::ImageRgb8(buf) => (ColorType::Rgb8, buf.into_raw()),
        DynamicImage::ImageRgba8(buf) => (ColorType::Rgba8, buf.into_raw()),
        DynamicImage::ImageRgb16(buf) => (
            ColorType::Rgb16,
            buf.into_raw()
                .into_iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
        ),
        other => (ColorType::Rgba8, other.into_rgba8().into_raw()),
    }
}

fn color_type_to_id(ct: ColorType) -> Result<u8> {
    Ok(match ct {
        ColorType::L8 => 1,
        ColorType::L16 => 2,
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        ColorType::Rgb16 => 5,
        _ => bail!("Unsupported color type: {:?}", ct),
    })
}

fn storage_color_type(color_type: ColorType, downcast_16_to_8: bool) -> ColorType {
    if !downcast_16_to_8 {
        return color_type;
    }

    match color_type {
        ColorType::L16 => ColorType::L8,
        ColorType::Rgb16 => ColorType::Rgb8,
        other => other,
    }
}

fn bytes_per_pixel(ct: &ColorType) -> Result<u32> {
    Ok(match ct {
        ColorType::L8 => 1,
        ColorType::L16 => 2,
        ColorType::Rgb8 => 3,
        ColorType::Rgba8 => 4,
        ColorType::Rgb16 => 6,
        _ => bail!("Unsupported color type {:?}", ct),
    })
}

fn bytes_per_pixel_from_id(id: u8) -> Result<u32> {
    Ok(match id {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        _ => bail!("Unsupported color type id {}", id),
    })
}

fn sample_layout(color_type: ColorType) -> Result<(usize, usize)> {
    Ok(match color_type {
        ColorType::L8 => (1, 1),
        ColorType::L16 => (1, 2),
        ColorType::Rgb8 => (3, 1),
        ColorType::Rgba8 => (4, 1),
        ColorType::Rgb16 => (3, 2),
        _ => bail!("Unsupported color type {:?}", color_type),
    })
}

fn convert_tile_for_storage(data: &[u8], input_color_type: ColorType, output_color_type: ColorType) -> Result<Vec<u8>> {
    if input_color_type == output_color_type {
        return Ok(data.to_vec());
    }

    match (input_color_type, output_color_type) {
        (ColorType::L16, ColorType::L8) | (ColorType::Rgb16, ColorType::Rgb8) => downcast_u16_bytes_to_u8(data),
        _ => bail!(
            "Unsupported storage conversion from {:?} to {:?}",
            input_color_type,
            output_color_type
        ),
    }
}

fn downcast_u16_bytes_to_u8(data: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes_to_u16_vec(data)?
        .into_iter()
        .map(|value| ((value as u32 + 128) / 257) as u8)
        .collect())
}

fn extract_tile(img: &TiffImage, tx: u32, ty: u32, ts: u32) -> Result<Vec<u8>> {
    let bpp = bytes_per_pixel(&img.color_type)?;
    let start_x = tx * ts;
    let start_y = ty * ts;
    let (tile_w, tile_h) = tile_extent(img.width, img.height, ts, tx, ty);
    let end_y = start_y + tile_h;

    let mut out = Vec::with_capacity((tile_w * tile_h * bpp) as usize);
    for y in start_y..end_y {
        let row_start = ((y * img.width + start_x) * bpp) as usize;
        let row_end = row_start + (tile_w * bpp) as usize;
        out.extend_from_slice(&img.data[row_start..row_end]);
    }
    Ok(out)
}

fn blit_tile(
    out: &mut [u8],
    tile: &[u8],
    w: u32,
    h: u32,
    ts: u32,
    bpp: u32,
    tx: u32,
    ty: u32,
) -> Result<()> {
    let start_x = tx * ts;
    let start_y = ty * ts;
    let end_x = (start_x + ts).min(w);
    let end_y = (start_y + ts).min(h);
    let tile_w = end_x - start_x;
    let tile_h = end_y - start_y;

    for row in 0..tile_h {
        let dst_off = (((start_y + row) * w + start_x) * bpp) as usize;
        let src_off = (row * tile_w * bpp) as usize;
        let len = (tile_w * bpp) as usize;
        out[dst_off..dst_off + len].copy_from_slice(&tile[src_off..src_off + len]);
    }
    Ok(())
}

fn blit_tile_region(
    out: &mut [u8],
    tile: &[u8],
    region_w: u32,
    region_h: u32,
    region_x: u32,
    region_y: u32,
    tile_size: u32,
    tile_w: u32,
    tile_h: u32,
    bpp: u32,
    tx: u32,
    ty: u32,
) -> Result<()> {
    let tile_x = tx * tile_size;
    let tile_y = ty * tile_size;
    let copy_x0 = tile_x.max(region_x);
    let copy_y0 = tile_y.max(region_y);
    let copy_x1 = (tile_x + tile_w).min(region_x + region_w);
    let copy_y1 = (tile_y + tile_h).min(region_y + region_h);

    if copy_x0 >= copy_x1 || copy_y0 >= copy_y1 {
        return Ok(());
    }

    let copy_w = copy_x1 - copy_x0;
    let copy_h = copy_y1 - copy_y0;
    for row in 0..copy_h {
        let src_x = copy_x0 - tile_x;
        let src_y = (copy_y0 - tile_y) + row;
        let dst_x = copy_x0 - region_x;
        let dst_y = (copy_y0 - region_y) + row;
        let src_off = ((src_y * tile_w + src_x) * bpp) as usize;
        let dst_off = ((dst_y * region_w + dst_x) * bpp) as usize;
        let len = (copy_w * bpp) as usize;
        out[dst_off..dst_off + len].copy_from_slice(&tile[src_off..src_off + len]);
    }
    Ok(())
}

fn tile_extent(width: u32, height: u32, tile_size: u32, tx: u32, ty: u32) -> (u32, u32) {
    let start_x = tx * tile_size;
    let start_y = ty * tile_size;
    (
        (start_x + tile_size).min(width) - start_x,
        (start_y + tile_size).min(height) - start_y,
    )
}

fn color_transform_mode_for_encode(enabled: bool, color_type: ColorType) -> ColorTransformMode {
    if !enabled {
        return ColorTransformMode::None;
    }

    match color_type {
        ColorType::Rgb8 => ColorTransformMode::Rct,
        ColorType::Rgb16 => ColorTransformMode::Rgb16DeltaG,
        _ => ColorTransformMode::None,
    }
}

fn color_transform_mode_from_header(flags: u16, color_type: u8) -> ColorTransformMode {
    if (flags & FLAG_COLOR_RGB16_DELTA_G) != 0 && color_type == 5 {
        ColorTransformMode::Rgb16DeltaG
    } else if (flags & FLAG_COLOR_RCT) != 0 && matches!(color_type, 3 | 5) {
        ColorTransformMode::Rct
    } else {
        ColorTransformMode::None
    }
}

fn color_transform_flags(mode: ColorTransformMode) -> u16 {
    match mode {
        ColorTransformMode::None => 0,
        ColorTransformMode::Rct => FLAG_COLOR_RCT,
        ColorTransformMode::Rgb16DeltaG => FLAG_COLOR_RGB16_DELTA_G,
    }
}

fn apply_color_transform_forward(mode: ColorTransformMode, color_type: ColorType, tile: &mut [u8]) {
    match mode {
        ColorTransformMode::None => {}
        ColorTransformMode::Rct => match color_type {
            ColorType::Rgb8 => rct_forward_rgb8(tile),
            ColorType::Rgb16 => rct_forward_rgb16(tile),
            _ => {}
        },
        ColorTransformMode::Rgb16DeltaG => rgb16_delta_g_forward(tile),
    }
}

fn apply_color_transform_inverse(mode: ColorTransformMode, color_type: u8, tile: &mut [u8]) {
    match mode {
        ColorTransformMode::None => {}
        ColorTransformMode::Rct => match color_type {
            3 => rct_inverse_rgb8(tile),
            5 => rct_inverse_rgb16(tile),
            _ => {}
        },
        ColorTransformMode::Rgb16DeltaG => rgb16_delta_g_inverse(tile),
    }
}

fn downsample_half(img: &TiffImage) -> Result<TiffImage> {
    let new_w = img.width.div_ceil(2);
    let new_h = img.height.div_ceil(2);
    let (channels, sample_bytes) = sample_layout(img.color_type)?;
    let mut out = Vec::with_capacity((new_w * new_h * bytes_per_pixel(&img.color_type)?) as usize);

    for y in 0..new_h {
        for x in 0..new_w {
            for ch in 0..channels {
                if sample_bytes == 1 {
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for sy in 0..2 {
                        for sx in 0..2 {
                            let src_x = x * 2 + sx;
                            let src_y = y * 2 + sy;
                            if src_x < img.width && src_y < img.height {
                                let idx = ((src_y * img.width + src_x) as usize * channels) + ch;
                                sum += img.data[idx] as u32;
                                count += 1;
                            }
                        }
                    }
                    out.push((sum / count) as u8);
                } else {
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for sy in 0..2 {
                        for sx in 0..2 {
                            let src_x = x * 2 + sx;
                            let src_y = y * 2 + sy;
                            if src_x < img.width && src_y < img.height {
                                let pixel = (src_y * img.width + src_x) as usize;
                                let off = pixel * channels * 2 + ch * 2;
                                let val = u16::from_le_bytes([img.data[off], img.data[off + 1]]) as u32;
                                sum += val;
                                count += 1;
                            }
                        }
                    }
                    out.extend_from_slice(&((sum / count) as u16).to_le_bytes());
                }
            }
        }
    }

    Ok(TiffImage {
        width: new_w,
        height: new_h,
        color_type: img.color_type,
        data: out,
        xdpi: img.xdpi.map(|v| v / 2.0),
        ydpi: img.ydpi.map(|v| v / 2.0),
        icc: img.icc.clone(),
    })
}

fn compress_tile(
    kind: CompressionType,
    color_type: ColorType,
    data: &[u8],
    tile_extent: (u32, u32),
    zstd_level: i32,
) -> Result<TileCompressionResult> {
    let comp = match kind {
        CompressionType::None => TileCompressionResult {
            comp: data.to_vec(),
            adaptive_mode: None,
        },
        CompressionType::RLE => TileCompressionResult {
            comp: rle_compress(data)?,
            adaptive_mode: None,
        },
        CompressionType::Delta => TileCompressionResult {
            comp: rle_compress(&delta_forward_for_color(data, color_type)?)?,
            adaptive_mode: None,
        },
        CompressionType::Predictive => TileCompressionResult {
            comp: rle_compress(&predictive_forward_for_color(data, color_type)?)?,
            adaptive_mode: None,
        },
        CompressionType::LZ77 => TileCompressionResult {
            comp: lz77_compress(data)?,
            adaptive_mode: None,
        },
        CompressionType::Zstd => TileCompressionResult {
            comp: zstd::bulk::compress(data, zstd_level)?,
            adaptive_mode: None,
        },
        CompressionType::Lz4 => TileCompressionResult {
            comp: lz4_flex::block::compress_prepend_size(data),
            adaptive_mode: None,
        },
        CompressionType::Adaptive => compress_tile_adaptive(color_type, data, tile_extent, zstd_level)?,
    };
    Ok(comp)
}

fn compress_prepared_tile(
    kind: CompressionType,
    color_type: ColorType,
    tile: Vec<u8>,
    tile_extent: (u32, u32),
    zstd_level: i32,
) -> Result<CompTile> {
    let comp = compress_tile(kind, color_type, &tile, tile_extent, zstd_level)?;
    Ok(CompTile {
        comp: comp.comp,
        orig_len: tile.len() as u32,
        crc: crc32(&tile),
        adaptive_mode: comp.adaptive_mode,
    })
}

fn streaming_batch_tiles(tile_size: u32, bytes_per_pixel: u32) -> usize {
    if let Some(override_tiles) = std::env::var_os("CTI_BATCH_TILES")
        .and_then(|value| value.to_str().map(str::trim).map(str::to_owned))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| value > 0)
    {
        return override_tiles;
    }

    let threads = rayon::current_num_threads().max(1);
    let max_tile_bytes = (tile_size as usize)
        .saturating_mul(tile_size as usize)
        .saturating_mul(bytes_per_pixel as usize)
        .max(1);
    let target_batch_bytes = 64usize * 1024 * 1024;
    let memory_limited = (target_batch_bytes / max_tile_bytes).max(1);
    memory_limited.min(threads * 2).max(1)
}

fn compress_tile_adaptive(
    color_type: ColorType,
    data: &[u8],
    tile_extent: (u32, u32),
    zstd_level: i32,
) -> Result<TileCompressionResult> {
    let mut best_mode = AdaptiveTileMode::ZstdRaw;
    let mut best_comp = zstd::bulk::compress(data, zstd_level)?;

    if matches!(color_type, ColorType::L16 | ColorType::Rgb16) {
        let channels = sample_layout(color_type)?.0;

        let shuffle = shuffle_u16_bytes(data)?;
        let shuffle_comp = zstd::bulk::compress(&shuffle, zstd_level)?;
        if shuffle_comp.len() < best_comp.len() {
            best_mode = AdaptiveTileMode::ZstdShuffle16;
            best_comp = shuffle_comp;
        }

        let delta = delta_forward_u16(data, channels)?;
        let delta_score = residual_score_u16(&delta, channels)?;
        let delta_comp = zstd::bulk::compress(&delta, zstd_level)?;
        if delta_comp.len() < best_comp.len() {
            best_mode = AdaptiveTileMode::ZstdDelta16;
            best_comp = delta_comp;
        }

        let predict = predictive_forward_u16(data, channels)?;
        let predict_score = residual_score_u16(&predict, channels * 2)?;
        if predict_score <= delta_score {
            let predict_comp = zstd::bulk::compress(&predict, zstd_level)?;
            if predict_comp.len() < best_comp.len() {
                best_mode = AdaptiveTileMode::ZstdPredict16;
                best_comp = predict_comp;
            }
        }

        let gradient = gradient_forward_u16(data, tile_extent.0 as usize, channels)?;
        let gradient_score =
            residual_score_u16(&gradient, tile_extent.0 as usize * channels + channels)?;
        let best_linear_score = predict_score.min(delta_score);
        if gradient_score * 100 <= best_linear_score * 95 {
            let gradient_comp = zstd::bulk::compress(&gradient, zstd_level)?;
            if gradient_comp.len() * 100 <= best_comp.len() * 97 {
                best_mode = AdaptiveTileMode::ZstdGradient16;
                best_comp = gradient_comp;
            }
        }

    }

    let lz4_raw = lz4_flex::block::compress_prepend_size(data);
    if lz4_raw.len() * 100 <= best_comp.len() * 105 {
        best_mode = AdaptiveTileMode::Lz4Raw;
        best_comp = lz4_raw;
    }

    Ok(TileCompressionResult {
        comp: best_comp,
        adaptive_mode: Some(best_mode),
    })
}

fn decompress_tile_with_size(
    kind: u8,
    comp: &[u8],
    original_size: usize,
    color_type: u8,
    tile_extent: (u32, u32),
    adaptive_mode: Option<AdaptiveTileMode>,
) -> Result<Vec<u8>> {
    match kind {
        0 => Ok(comp.to_vec()),
        1 => rle_decompress(comp),
        2 => lz77_decompress(comp),
        3 => {
            let d = rle_decompress(comp)?;
            delta_inverse_for_color(&d, color_type)
        }
        4 => {
            let d = rle_decompress(comp)?;
            predictive_inverse_for_color(&d, color_type)
        }
        10 => zstd::bulk::decompress(comp, original_size)
            .map_err(|e| anyhow!("zstd decompress failed: {e}")),
        11 => lz4_flex::block::decompress_size_prepended(comp).map_err(|e| anyhow!(e)),
        250 => {
            let mode = adaptive_mode.context("Adaptive tile is missing mode metadata")?;
            match mode {
                AdaptiveTileMode::ZstdRaw => zstd::bulk::decompress(comp, original_size)
                    .map_err(|e| anyhow!("zstd decompress failed: {e}")),
                AdaptiveTileMode::ZstdDelta16 => {
                    let d = zstd::bulk::decompress(comp, original_size)
                        .map_err(|e| anyhow!("zstd decompress failed: {e}"))?;
                    delta_inverse_for_color(&d, color_type)
                }
                AdaptiveTileMode::ZstdPredict16 => {
                    let d = zstd::bulk::decompress(comp, original_size)
                        .map_err(|e| anyhow!("zstd decompress failed: {e}"))?;
                    predictive_inverse_for_color(&d, color_type)
                }
                AdaptiveTileMode::ZstdShuffle16 => {
                    let d = zstd::bulk::decompress(comp, original_size)
                        .map_err(|e| anyhow!("zstd decompress failed: {e}"))?;
                    unshuffle_u16_bytes(&d)
                }
                AdaptiveTileMode::ZstdGradient16 => {
                    let d = zstd::bulk::decompress(comp, original_size)
                        .map_err(|e| anyhow!("zstd decompress failed: {e}"))?;
                    gradient_inverse_u16(
                        &d,
                        tile_extent.0 as usize,
                        bytes_per_pixel_from_id(color_type)? as usize / 2,
                    )
                }
                AdaptiveTileMode::Lz4Raw => {
                    lz4_flex::block::decompress_size_prepended(comp).map_err(|e| anyhow!(e))
                }
            }
        }
        _ => bail!("Unknown compression id {}", kind),
    }
}

fn rle_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        let val = data[i];
        let mut cnt = 1usize;
        while i + cnt < data.len() && data[i + cnt] == val && cnt < 255 {
            cnt += 1;
        }
        if cnt >= 4 {
            out.push(0xFF);
            out.push(TAG_RLE);
            out.push(cnt as u8);
            out.push(val);
            i += cnt;
        } else {
            for _ in 0..cnt {
                if val == 0xFF {
                    out.push(0xFF);
                    out.push(TAG_ESCAPE_FF);
                } else {
                    out.push(val);
                }
            }
            i += cnt;
        }
    }
    Ok(out)
}

fn rle_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        i += 1;
        if b != 0xFF {
            out.push(b);
            continue;
        }
        ensure!(i < data.len(), "RLE: truncated after 0xFF");
        let tag = data[i];
        i += 1;
        match tag {
            TAG_ESCAPE_FF => out.push(0xFF),
            TAG_RLE => {
                ensure!(i + 1 < data.len(), "RLE: truncated run");
                let count = data[i] as usize;
                let val = data[i + 1];
                i += 2;
                out.extend(std::iter::repeat_n(val, count));
            }
            TAG_LZ77 => bail!("RLE stream contains LZ77 tag"),
            _ => bail!("RLE unknown tag {}", tag),
        }
    }
    Ok(out)
}

fn delta_forward(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![];
    }
    let mut out = Vec::with_capacity(data.len());
    out.push(data[0]);
    for i in 1..data.len() {
        out.push(data[i].wrapping_sub(data[i - 1]));
    }
    out
}

fn delta_forward_for_color(data: &[u8], color_type: ColorType) -> Result<Vec<u8>> {
    match color_type {
        ColorType::L16 | ColorType::Rgb16 => delta_forward_u16(data, sample_layout(color_type)?.0),
        _ => Ok(delta_forward(data)),
    }
}

fn delta_inverse(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![];
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = data[0];
    out.push(prev);
    for &value in data.iter().skip(1) {
        let v = prev.wrapping_add(value);
        out.push(v);
        prev = v;
    }
    out
}

fn delta_inverse_for_color(data: &[u8], color_type: u8) -> Result<Vec<u8>> {
    match color_type {
        2 | 5 => delta_inverse_u16(data, bytes_per_pixel_from_id(color_type)? as usize / 2),
        _ => Ok(delta_inverse(data)),
    }
}

fn predictive_forward(data: &[u8]) -> Vec<u8> {
    if data.len() < 3 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    out.push(data[0]);
    out.push(data[1]);
    for i in 2..data.len() {
        let p = data[i - 1].wrapping_add(data[i - 1].wrapping_sub(data[i - 2]));
        out.push(data[i].wrapping_sub(p));
    }
    out
}

fn predictive_forward_for_color(data: &[u8], color_type: ColorType) -> Result<Vec<u8>> {
    match color_type {
        ColorType::L16 | ColorType::Rgb16 => predictive_forward_u16(data, sample_layout(color_type)?.0),
        _ => Ok(predictive_forward(data)),
    }
}

fn predictive_inverse(data: &[u8]) -> Vec<u8> {
    if data.len() < 3 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut a0, mut a1) = (data[0], data[1]);
    out.push(a0);
    out.push(a1);
    for &value in data.iter().skip(2) {
        let p = a1.wrapping_add(a1.wrapping_sub(a0));
        let v = p.wrapping_add(value);
        out.push(v);
        a0 = a1;
        a1 = v;
    }
    out
}

fn predictive_inverse_for_color(data: &[u8], color_type: u8) -> Result<Vec<u8>> {
    match color_type {
        2 | 5 => predictive_inverse_u16(data, bytes_per_pixel_from_id(color_type)? as usize / 2),
        _ => Ok(predictive_inverse(data)),
    }
}

fn delta_forward_u16(data: &[u8], channels: usize) -> Result<Vec<u8>> {
    let samples = bytes_to_u16_vec(data)?;
    let mut out = vec![0u16; samples.len()];

    let head = channels.min(samples.len());
    out[..head].copy_from_slice(&samples[..head]);

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && channels > 0 && samples.len() > channels {
            unsafe {
                delta_forward_u16_avx2(&samples, channels, &mut out);
            }
            return Ok(u16_vec_to_bytes(&out));
        }
    }

    for i in channels..samples.len() {
        out[i] = samples[i].wrapping_sub(samples[i - channels]);
    }
    Ok(u16_vec_to_bytes(&out))
}

fn delta_inverse_u16(data: &[u8], channels: usize) -> Result<Vec<u8>> {
    let residuals = bytes_to_u16_vec(data)?;
    let mut out = vec![0u16; residuals.len()];
    for i in 0..residuals.len() {
        out[i] = if i < channels {
            residuals[i]
        } else {
            out[i - channels].wrapping_add(residuals[i])
        };
    }
    Ok(u16_vec_to_bytes(&out))
}

fn predictive_forward_u16(data: &[u8], channels: usize) -> Result<Vec<u8>> {
    let samples = bytes_to_u16_vec(data)?;
    let mut out = vec![0u16; samples.len()];

    let head = (channels * 2).min(samples.len());
    out[..head].copy_from_slice(&samples[..head]);

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && channels > 0 && samples.len() > channels * 2 {
            unsafe {
                predictive_forward_u16_avx2(&samples, channels, &mut out);
            }
            return Ok(u16_vec_to_bytes(&out));
        }
    }

    for i in channels * 2..samples.len() {
        let prev = samples[i - channels];
        let prev_prev = samples[i - channels * 2];
        let predicted = prev.wrapping_add(prev.wrapping_sub(prev_prev));
        out[i] = samples[i].wrapping_sub(predicted);
    }
    Ok(u16_vec_to_bytes(&out))
}

fn predictive_inverse_u16(data: &[u8], channels: usize) -> Result<Vec<u8>> {
    let residuals = bytes_to_u16_vec(data)?;
    let mut out = vec![0u16; residuals.len()];
    for i in 0..residuals.len() {
        out[i] = if i < channels * 2 {
            residuals[i]
        } else {
            let prev = out[i - channels];
            let prev_prev = out[i - channels * 2];
            let predicted = prev.wrapping_add(prev.wrapping_sub(prev_prev));
            predicted.wrapping_add(residuals[i])
        };
    }
    Ok(u16_vec_to_bytes(&out))
}

fn gradient_forward_u16(data: &[u8], width: usize, channels: usize) -> Result<Vec<u8>> {
    ensure!(width > 0, "gradient predictor requires non-zero width");
    let samples = bytes_to_u16_vec(data)?;
    let row_stride = width * channels;
    ensure!(row_stride > 0, "gradient predictor requires non-zero row stride");
    ensure!(
        samples.len() % row_stride == 0,
        "gradient predictor tile size does not match row stride"
    );

    let mut out = vec![0u16; samples.len()];
    for i in 0..samples.len() {
        let row_offset = i % row_stride;
        out[i] = if i < row_stride {
            if row_offset < channels {
                samples[i]
            } else {
                samples[i].wrapping_sub(samples[i - channels])
            }
        } else if row_offset < channels {
            samples[i].wrapping_sub(samples[i - row_stride])
        } else {
            let left = samples[i - channels];
            let top = samples[i - row_stride];
            let top_left = samples[i - row_stride - channels];
            samples[i].wrapping_sub(jpegls_predict_u16(left, top, top_left))
        };
    }
    Ok(u16_vec_to_bytes(&out))
}

fn gradient_inverse_u16(data: &[u8], width: usize, channels: usize) -> Result<Vec<u8>> {
    ensure!(width > 0, "gradient predictor requires non-zero width");
    let residuals = bytes_to_u16_vec(data)?;
    let row_stride = width * channels;
    ensure!(row_stride > 0, "gradient predictor requires non-zero row stride");
    ensure!(
        residuals.len() % row_stride == 0,
        "gradient predictor tile size does not match row stride"
    );

    let mut out = vec![0u16; residuals.len()];
    for i in 0..residuals.len() {
        let row_offset = i % row_stride;
        out[i] = if i < row_stride {
            if row_offset < channels {
                residuals[i]
            } else {
                out[i - channels].wrapping_add(residuals[i])
            }
        } else if row_offset < channels {
            out[i - row_stride].wrapping_add(residuals[i])
        } else {
            let left = out[i - channels];
            let top = out[i - row_stride];
            let top_left = out[i - row_stride - channels];
            jpegls_predict_u16(left, top, top_left).wrapping_add(residuals[i])
        };
    }
    Ok(u16_vec_to_bytes(&out))
}

fn jpegls_predict_u16(left: u16, top: u16, top_left: u16) -> u16 {
    if top_left >= left.max(top) {
        left.min(top)
    } else if top_left <= left.min(top) {
        left.max(top)
    } else {
        (left as u32 + top as u32 - top_left as u32) as u16
    }
}

fn shuffle_u16_bytes(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() % 2 == 0, "16-bit shuffle requires even byte length");
    let samples = data.len() / 2;
    let mut out = vec![0u8; data.len()];

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") {
            unsafe {
                shuffle_u16_bytes_avx2(data, &mut out);
            }
            return Ok(out);
        }
    }

    for i in 0..samples {
        out[i] = data[i * 2];
        out[samples + i] = data[i * 2 + 1];
    }
    Ok(out)
}

fn unshuffle_u16_bytes(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() % 2 == 0, "16-bit unshuffle requires even byte length");
    let samples = data.len() / 2;
    let mut out = vec![0u8; data.len()];

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") {
            unsafe {
                unshuffle_u16_bytes_avx2(data, &mut out);
            }
            return Ok(out);
        }
    }

    for i in 0..samples {
        out[i * 2] = data[i];
        out[i * 2 + 1] = data[samples + i];
    }
    Ok(out)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn delta_forward_u16_avx2(samples: &[u16], channels: usize, out: &mut [u16]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let mut i = channels;
    while i + 16 <= samples.len() {
        let cur = unsafe { _mm256_loadu_si256(samples.as_ptr().add(i) as *const __m256i) };
        let prev = unsafe { _mm256_loadu_si256(samples.as_ptr().add(i - channels) as *const __m256i) };
        let delta = _mm256_sub_epi16(cur, prev);
        unsafe { _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, delta) };
        i += 16;
    }

    while i < samples.len() {
        out[i] = samples[i].wrapping_sub(samples[i - channels]);
        i += 1;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn predictive_forward_u16_avx2(samples: &[u16], channels: usize, out: &mut [u16]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let mut i = channels * 2;
    while i + 16 <= samples.len() {
        let cur = unsafe { _mm256_loadu_si256(samples.as_ptr().add(i) as *const __m256i) };
        let prev = unsafe { _mm256_loadu_si256(samples.as_ptr().add(i - channels) as *const __m256i) };
        let prev_prev =
            unsafe { _mm256_loadu_si256(samples.as_ptr().add(i - channels * 2) as *const __m256i) };
        let diff = _mm256_sub_epi16(prev, prev_prev);
        let predicted = _mm256_add_epi16(prev, diff);
        let residual = _mm256_sub_epi16(cur, predicted);
        unsafe { _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut __m256i, residual) };
        i += 16;
    }

    while i < samples.len() {
        let prev = samples[i - channels];
        let prev_prev = samples[i - channels * 2];
        let predicted = prev.wrapping_add(prev.wrapping_sub(prev_prev));
        out[i] = samples[i].wrapping_sub(predicted);
        i += 1;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn shuffle_u16_bytes_avx2(data: &[u8], out: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let samples = data.len() / 2;
    let low_mask = _mm_setr_epi8(0, 2, 4, 6, 8, 10, 12, 14, -1, -1, -1, -1, -1, -1, -1, -1);
    let high_mask = _mm_setr_epi8(1, 3, 5, 7, 9, 11, 13, 15, -1, -1, -1, -1, -1, -1, -1, -1);

    let mut sample_idx = 0usize;
    while sample_idx + 16 <= samples {
        let byte_idx = sample_idx * 2;
        let lo = unsafe { _mm_loadu_si128(data.as_ptr().add(byte_idx) as *const __m128i) };
        let hi = unsafe { _mm_loadu_si128(data.as_ptr().add(byte_idx + 16) as *const __m128i) };
        let lo_bytes_lo = _mm_shuffle_epi8(lo, low_mask);
        let lo_bytes_hi = _mm_shuffle_epi8(hi, low_mask);
        let hi_bytes_lo = _mm_shuffle_epi8(lo, high_mask);
        let hi_bytes_hi = _mm_shuffle_epi8(hi, high_mask);
        let lows = _mm_unpacklo_epi64(lo_bytes_lo, lo_bytes_hi);
        let highs = _mm_unpacklo_epi64(hi_bytes_lo, hi_bytes_hi);
        unsafe { _mm_storeu_si128(out.as_mut_ptr().add(sample_idx) as *mut __m128i, lows) };
        unsafe {
            _mm_storeu_si128(out.as_mut_ptr().add(samples + sample_idx) as *mut __m128i, highs)
        };
        sample_idx += 16;
    }

    while sample_idx < samples {
        out[sample_idx] = data[sample_idx * 2];
        out[samples + sample_idx] = data[sample_idx * 2 + 1];
        sample_idx += 1;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn unshuffle_u16_bytes_avx2(data: &[u8], out: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let samples = data.len() / 2;
    let mut sample_idx = 0usize;
    while sample_idx + 16 <= samples {
        let lows = unsafe { _mm_loadu_si128(data.as_ptr().add(sample_idx) as *const __m128i) };
        let highs =
            unsafe { _mm_loadu_si128(data.as_ptr().add(samples + sample_idx) as *const __m128i) };
        let first = _mm_unpacklo_epi8(lows, highs);
        let second = _mm_unpackhi_epi8(lows, highs);
        unsafe { _mm_storeu_si128(out.as_mut_ptr().add(sample_idx * 2) as *mut __m128i, first) };
        unsafe {
            _mm_storeu_si128(out.as_mut_ptr().add(sample_idx * 2 + 16) as *mut __m128i, second)
        };
        sample_idx += 16;
    }

    while sample_idx < samples {
        out[sample_idx * 2] = data[sample_idx];
        out[sample_idx * 2 + 1] = data[samples + sample_idx];
        sample_idx += 1;
    }
}

fn residual_score_u16(data: &[u8], warmup_samples: usize) -> Result<u64> {
    let residuals = bytes_to_u16_vec(data)?;
    let residuals = residuals.get(warmup_samples..).unwrap_or(&[]);
    Ok(sum_abs_u16_residuals(residuals))
}

fn sum_abs_u16_residuals(values: &[u16]) -> u64 {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // AVX2 gives us a cheap residual score used by the adaptive codec selector.
            unsafe {
                return sum_abs_u16_residuals_avx2(values);
            }
        }
    }
    values
        .iter()
        .map(|&value| (value as i16 as i32).unsigned_abs() as u64)
        .sum()
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn sum_abs_u16_residuals_avx2(values: &[u16]) -> u64 {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let mut acc = _mm256_setzero_si256();
    let zero = _mm256_setzero_si256();
    let mut chunks = values.chunks_exact(16);
    for chunk in &mut chunks {
        let vec = unsafe { _mm256_loadu_si256(chunk.as_ptr() as *const __m256i) };
        let abs = _mm256_abs_epi16(vec);
        let lo = _mm256_unpacklo_epi16(abs, zero);
        let hi = _mm256_unpackhi_epi16(abs, zero);
        acc = _mm256_add_epi32(acc, lo);
        acc = _mm256_add_epi32(acc, hi);
    }

    let mut lanes = [0i32; 8];
    unsafe { _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc) };
    let mut sum = lanes.iter().map(|&lane| lane as u64).sum::<u64>();
    for &value in chunks.remainder() {
        sum += (value as i16 as i32).unsigned_abs() as u64;
    }
    sum
}

fn lz77_compress(data: &[u8]) -> Result<Vec<u8>> {
    const WINDOW: usize = 4096;
    const MIN_MATCH: usize = 3;
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        let start = i.saturating_sub(WINDOW);
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        for j in start..i {
            let mut l = 0usize;
            while i + l < data.len() && j + l < i && data[j + l] == data[i + l] && l < 255 {
                l += 1;
            }
            if l >= MIN_MATCH && l > best_len {
                best_len = l;
                best_dist = i - j;
                if best_len == 255 {
                    break;
                }
            }
        }
        if best_len >= MIN_MATCH {
            out.push(0xFF);
            out.push(TAG_LZ77);
            out.push((best_dist >> 8) as u8);
            out.push((best_dist & 0xFF) as u8);
            out.push(best_len as u8);
            i += best_len;
        } else {
            let b = data[i];
            if b == 0xFF {
                out.push(0xFF);
                out.push(TAG_ESCAPE_FF);
            } else {
                out.push(b);
            }
            i += 1;
        }
    }
    Ok(out)
}

fn lz77_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        let b = data[i];
        i += 1;
        if b != 0xFF {
            out.push(b);
            continue;
        }
        ensure!(i < data.len(), "LZ77: truncated after 0xFF");
        let tag = data[i];
        i += 1;
        match tag {
            TAG_ESCAPE_FF => out.push(0xFF),
            TAG_RLE => {
                ensure!(i + 1 < data.len(), "LZ77: RLE tuple truncated");
                let count = data[i] as usize;
                let val = data[i + 1];
                i += 2;
                out.extend(std::iter::repeat_n(val, count));
            }
            TAG_LZ77 => {
                ensure!(i + 2 < data.len(), "LZ77: backref truncated");
                let dist = ((data[i] as usize) << 8) | (data[i + 1] as usize);
                let len = data[i + 2] as usize;
                i += 3;
                ensure!(dist > 0, "LZ77: distance zero");
                ensure!(len >= 3, "LZ77: len too small");
                ensure!(dist <= out.len(), "LZ77: distance beyond buffer");
                let start = out.len() - dist;
                for k in 0..len {
                    let byte = out[start + k];
                    out.push(byte);
                }
            }
            _ => bail!("LZ77: unknown tag {}", tag),
        }
    }
    Ok(out)
}

fn rct_forward_rgb8(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 3 {
            unsafe {
                rct_forward_rgb8_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(3) {
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        let y = (r + (g << 1) + b) >> 2;
        let cb = b - g;
        let cr = r - g;
        p[0] = y.clamp(0, 255) as u8;
        p[1] = (cb as i16 as i32 & 0xFF) as u8;
        p[2] = (cr as i16 as i32 & 0xFF) as u8;
    }
}

fn rct_inverse_rgb8(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 3 {
            unsafe {
                rct_inverse_rgb8_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(3) {
        let y = p[0] as i32;
        let cb = (p[1] as i8) as i32;
        let cr = (p[2] as i8) as i32;
        let g = y - ((cb + cr) >> 2);
        let r = cr + g;
        let b = cb + g;
        p[0] = r.clamp(0, 255) as u8;
        p[1] = g.clamp(0, 255) as u8;
        p[2] = b.clamp(0, 255) as u8;
    }
}

fn rct_forward_rgb16(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 6 {
            unsafe {
                rct_forward_rgb16_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(6) {
        let r = u16::from_le_bytes([p[0], p[1]]) as i32;
        let g = u16::from_le_bytes([p[2], p[3]]) as i32;
        let b = u16::from_le_bytes([p[4], p[5]]) as i32;
        let y = (r + (g << 1) + b) >> 2;
        let cb = b - g;
        let cr = r - g;
        p[0..2].copy_from_slice(&(y.clamp(0, 65535) as u16).to_le_bytes());
        p[2..4].copy_from_slice(&((cb as i32 & 0xFFFF) as u16).to_le_bytes());
        p[4..6].copy_from_slice(&((cr as i32 & 0xFFFF) as u16).to_le_bytes());
    }
}

fn rct_inverse_rgb16(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 6 {
            unsafe {
                rct_inverse_rgb16_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(6) {
        let y = u16::from_le_bytes([p[0], p[1]]) as i32;
        let cb = (u16::from_le_bytes([p[2], p[3]]) as i16) as i32;
        let cr = (u16::from_le_bytes([p[4], p[5]]) as i16) as i32;
        let g = y - ((cb + cr) >> 2);
        let r = cr + g;
        let b = cb + g;
        p[0..2].copy_from_slice(&(r.clamp(0, 65535) as u16).to_le_bytes());
        p[2..4].copy_from_slice(&(g.clamp(0, 65535) as u16).to_le_bytes());
        p[4..6].copy_from_slice(&(b.clamp(0, 65535) as u16).to_le_bytes());
    }
}

fn rgb16_delta_g_forward(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 6 {
            unsafe {
                rgb16_delta_g_forward_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(6) {
        let r = u16::from_le_bytes([p[0], p[1]]);
        let g = u16::from_le_bytes([p[2], p[3]]);
        let b = u16::from_le_bytes([p[4], p[5]]);
        let dr = r.wrapping_sub(g);
        let db = b.wrapping_sub(g);
        p[0..2].copy_from_slice(&g.to_le_bytes());
        p[2..4].copy_from_slice(&dr.to_le_bytes());
        p[4..6].copy_from_slice(&db.to_le_bytes());
    }
}

fn rgb16_delta_g_inverse(buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::is_x86_feature_detected!("avx2") && buf.len() >= 16 * 6 {
            unsafe {
                rgb16_delta_g_inverse_avx2(buf);
            }
            return;
        }
    }

    for p in buf.chunks_exact_mut(6) {
        let g = u16::from_le_bytes([p[0], p[1]]);
        let dr = u16::from_le_bytes([p[2], p[3]]);
        let db = u16::from_le_bytes([p[4], p[5]]);
        let r = g.wrapping_add(dr);
        let b = g.wrapping_add(db);
        p[0..2].copy_from_slice(&r.to_le_bytes());
        p[2..4].copy_from_slice(&g.to_le_bytes());
        p[4..6].copy_from_slice(&b.to_le_bytes());
    }
}

const DEFAULT_RGB8_PLANAR_BLOCK_PIXELS: usize = 2048;
const DEFAULT_RGB16_PLANAR_BLOCK_PIXELS: usize = 512;

fn rgb_planar_block_pixels(default_pixels: usize) -> usize {
    static RGB_PLANAR_BLOCK_PIXELS_OVERRIDE: OnceLock<Option<usize>> = OnceLock::new();
    RGB_PLANAR_BLOCK_PIXELS_OVERRIDE
        .get_or_init(|| {
        std::env::var("CTI_RGB_PLANAR_BLOCK_PIXELS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|value| value.clamp(64, 16_384))
        })
        .unwrap_or(default_pixels)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rct_forward_rgb8_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 3;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB8_PLANAR_BLOCK_PIXELS);
    let mut rs = vec![0u8; block_pixels];
    let mut gs = vec![0u8; block_pixels];
    let mut bs = vec![0u8; block_pixels];
    let mut ys = vec![0u8; block_pixels];
    let mut cbs = vec![0u8; block_pixels];
    let mut crs = vec![0u8; block_pixels];
    let low_byte_mask = _mm256_set1_epi16(0x00FF);

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 3;
            rs[lane] = buf[base];
            gs[lane] = buf[base + 1];
            bs[lane] = buf[base + 2];
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let r =
                unsafe { _mm256_cvtepu8_epi16(_mm_loadu_si128(rs.as_ptr().add(lane) as *const __m128i)) };
            let g =
                unsafe { _mm256_cvtepu8_epi16(_mm_loadu_si128(gs.as_ptr().add(lane) as *const __m128i)) };
            let b =
                unsafe { _mm256_cvtepu8_epi16(_mm_loadu_si128(bs.as_ptr().add(lane) as *const __m128i)) };
            let y = _mm256_srli_epi16(
                _mm256_add_epi16(_mm256_add_epi16(r, _mm256_slli_epi16(g, 1)), b),
                2,
            );
            let cb = _mm256_and_si256(_mm256_sub_epi16(b, g), low_byte_mask);
            let cr = _mm256_and_si256(_mm256_sub_epi16(r, g), low_byte_mask);

            let y_lo = _mm256_castsi256_si128(y);
            let y_hi = _mm256_extracti128_si256(y, 1);
            let cb_lo = _mm256_castsi256_si128(cb);
            let cb_hi = _mm256_extracti128_si256(cb, 1);
            let cr_lo = _mm256_castsi256_si128(cr);
            let cr_hi = _mm256_extracti128_si256(cr, 1);
            let y8 = _mm_packus_epi16(y_lo, y_hi);
            let cb8 = _mm_packus_epi16(cb_lo, cb_hi);
            let cr8 = _mm_packus_epi16(cr_lo, cr_hi);

            unsafe { _mm_storeu_si128(ys.as_mut_ptr().add(lane) as *mut __m128i, y8) };
            unsafe { _mm_storeu_si128(cbs.as_mut_ptr().add(lane) as *mut __m128i, cb8) };
            unsafe { _mm_storeu_si128(crs.as_mut_ptr().add(lane) as *mut __m128i, cr8) };
            lane += 16;
        }

        while lane < count {
            let r = rs[lane] as i32;
            let g = gs[lane] as i32;
            let b = bs[lane] as i32;
            let y = (r + (g << 1) + b) >> 2;
            let cb = b - g;
            let cr = r - g;
            ys[lane] = y.clamp(0, 255) as u8;
            cbs[lane] = (cb as i16 as i32 & 0xFF) as u8;
            crs[lane] = (cr as i16 as i32 & 0xFF) as u8;
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 3;
            buf[base] = ys[lane];
            buf[base + 1] = cbs[lane];
            buf[base + 2] = crs[lane];
        }

        pixel_idx += count;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rct_inverse_rgb8_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 3;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB8_PLANAR_BLOCK_PIXELS);
    let mut ys = vec![0u8; block_pixels];
    let mut cbs = vec![0u8; block_pixels];
    let mut crs = vec![0u8; block_pixels];
    let mut rs = vec![0u8; block_pixels];
    let mut gs = vec![0u8; block_pixels];
    let mut bs = vec![0u8; block_pixels];

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 3;
            ys[lane] = buf[base];
            cbs[lane] = buf[base + 1];
            crs[lane] = buf[base + 2];
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let y =
                unsafe { _mm256_cvtepu8_epi16(_mm_loadu_si128(ys.as_ptr().add(lane) as *const __m128i)) };
            let cb = unsafe {
                _mm256_cvtepi8_epi16(_mm_loadu_si128(cbs.as_ptr().add(lane) as *const __m128i))
            };
            let cr = unsafe {
                _mm256_cvtepi8_epi16(_mm_loadu_si128(crs.as_ptr().add(lane) as *const __m128i))
            };
            let g = _mm256_sub_epi16(y, _mm256_srai_epi16(_mm256_add_epi16(cb, cr), 2));
            let r = _mm256_add_epi16(cr, g);
            let b = _mm256_add_epi16(cb, g);

            let r_lo = _mm256_castsi256_si128(r);
            let r_hi = _mm256_extracti128_si256(r, 1);
            let g_lo = _mm256_castsi256_si128(g);
            let g_hi = _mm256_extracti128_si256(g, 1);
            let b_lo = _mm256_castsi256_si128(b);
            let b_hi = _mm256_extracti128_si256(b, 1);
            let r8 = _mm_packus_epi16(r_lo, r_hi);
            let g8 = _mm_packus_epi16(g_lo, g_hi);
            let b8 = _mm_packus_epi16(b_lo, b_hi);

            unsafe { _mm_storeu_si128(rs.as_mut_ptr().add(lane) as *mut __m128i, r8) };
            unsafe { _mm_storeu_si128(gs.as_mut_ptr().add(lane) as *mut __m128i, g8) };
            unsafe { _mm_storeu_si128(bs.as_mut_ptr().add(lane) as *mut __m128i, b8) };
            lane += 16;
        }

        while lane < count {
            let y = ys[lane] as i32;
            let cb = (cbs[lane] as i8) as i32;
            let cr = (crs[lane] as i8) as i32;
            let g = y - ((cb + cr) >> 2);
            let r = cr + g;
            let b = cb + g;
            rs[lane] = r.clamp(0, 255) as u8;
            gs[lane] = g.clamp(0, 255) as u8;
            bs[lane] = b.clamp(0, 255) as u8;
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 3;
            buf[base] = rs[lane];
            buf[base + 1] = gs[lane];
            buf[base + 2] = bs[lane];
        }

        pixel_idx += count;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rct_forward_rgb16_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 6;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB16_PLANAR_BLOCK_PIXELS);
    let mut c0 = vec![0u16; block_pixels];
    let mut c1 = vec![0u16; block_pixels];
    let mut c2 = vec![0u16; block_pixels];

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            c0[lane] = u16::from_le_bytes([buf[base], buf[base + 1]]);
            c1[lane] = u16::from_le_bytes([buf[base + 2], buf[base + 3]]);
            c2[lane] = u16::from_le_bytes([buf[base + 4], buf[base + 5]]);
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let r = unsafe { _mm256_loadu_si256(c0.as_ptr().add(lane) as *const __m256i) };
            let g = unsafe { _mm256_loadu_si256(c1.as_ptr().add(lane) as *const __m256i) };
            let b = unsafe { _mm256_loadu_si256(c2.as_ptr().add(lane) as *const __m256i) };
            let y = _mm256_srli_epi16(
                _mm256_add_epi16(_mm256_add_epi16(r, _mm256_slli_epi16(g, 1)), b),
                2,
            );
            let cb = _mm256_sub_epi16(b, g);
            let cr = _mm256_sub_epi16(r, g);
            unsafe { _mm256_storeu_si256(c0.as_mut_ptr().add(lane) as *mut __m256i, y) };
            unsafe { _mm256_storeu_si256(c1.as_mut_ptr().add(lane) as *mut __m256i, cb) };
            unsafe { _mm256_storeu_si256(c2.as_mut_ptr().add(lane) as *mut __m256i, cr) };
            lane += 16;
        }

        while lane < count {
            let r = c0[lane] as i32;
            let g = c1[lane] as i32;
            let b = c2[lane] as i32;
            c0[lane] = ((r + (g << 1) + b) >> 2) as u16;
            c1[lane] = (b - g) as u16;
            c2[lane] = (r - g) as u16;
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            buf[base..base + 2].copy_from_slice(&c0[lane].to_le_bytes());
            buf[base + 2..base + 4].copy_from_slice(&c1[lane].to_le_bytes());
            buf[base + 4..base + 6].copy_from_slice(&c2[lane].to_le_bytes());
        }

        pixel_idx += count;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rct_inverse_rgb16_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 6;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB16_PLANAR_BLOCK_PIXELS);
    let mut c0 = vec![0u16; block_pixels];
    let mut c1 = vec![0u16; block_pixels];
    let mut c2 = vec![0u16; block_pixels];

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            c0[lane] = u16::from_le_bytes([buf[base], buf[base + 1]]);
            c1[lane] = u16::from_le_bytes([buf[base + 2], buf[base + 3]]);
            c2[lane] = u16::from_le_bytes([buf[base + 4], buf[base + 5]]);
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let y = unsafe { _mm256_loadu_si256(c0.as_ptr().add(lane) as *const __m256i) };
            let cb_bits = unsafe { _mm256_loadu_si256(c1.as_ptr().add(lane) as *const __m256i) };
            let cr_bits = unsafe { _mm256_loadu_si256(c2.as_ptr().add(lane) as *const __m256i) };
            let cb = cb_bits;
            let cr = cr_bits;
            let g = _mm256_sub_epi16(y, _mm256_srai_epi16(_mm256_add_epi16(cb, cr), 2));
            let r = _mm256_add_epi16(cr, g);
            let b = _mm256_add_epi16(cb, g);
            unsafe { _mm256_storeu_si256(c0.as_mut_ptr().add(lane) as *mut __m256i, r) };
            unsafe { _mm256_storeu_si256(c1.as_mut_ptr().add(lane) as *mut __m256i, g) };
            unsafe { _mm256_storeu_si256(c2.as_mut_ptr().add(lane) as *mut __m256i, b) };
            lane += 16;
        }

        while lane < count {
            let y = c0[lane] as i32;
            let cb = c1[lane] as i16 as i32;
            let cr = c2[lane] as i16 as i32;
            let g = y - ((cb + cr) >> 2);
            let r = cr + g;
            let b = cb + g;
            c0[lane] = r.clamp(0, 65535) as u16;
            c1[lane] = g.clamp(0, 65535) as u16;
            c2[lane] = b.clamp(0, 65535) as u16;
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            buf[base..base + 2].copy_from_slice(&c0[lane].to_le_bytes());
            buf[base + 2..base + 4].copy_from_slice(&c1[lane].to_le_bytes());
            buf[base + 4..base + 6].copy_from_slice(&c2[lane].to_le_bytes());
        }

        pixel_idx += count;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rgb16_delta_g_forward_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 6;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB16_PLANAR_BLOCK_PIXELS);
    let mut c0 = vec![0u16; block_pixels];
    let mut c1 = vec![0u16; block_pixels];
    let mut c2 = vec![0u16; block_pixels];

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            c0[lane] = u16::from_le_bytes([buf[base], buf[base + 1]]);
            c1[lane] = u16::from_le_bytes([buf[base + 2], buf[base + 3]]);
            c2[lane] = u16::from_le_bytes([buf[base + 4], buf[base + 5]]);
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let r = unsafe { _mm256_loadu_si256(c0.as_ptr().add(lane) as *const __m256i) };
            let g = unsafe { _mm256_loadu_si256(c1.as_ptr().add(lane) as *const __m256i) };
            let b = unsafe { _mm256_loadu_si256(c2.as_ptr().add(lane) as *const __m256i) };
            let dr = _mm256_sub_epi16(r, g);
            let db = _mm256_sub_epi16(b, g);
            unsafe { _mm256_storeu_si256(c0.as_mut_ptr().add(lane) as *mut __m256i, g) };
            unsafe { _mm256_storeu_si256(c1.as_mut_ptr().add(lane) as *mut __m256i, dr) };
            unsafe { _mm256_storeu_si256(c2.as_mut_ptr().add(lane) as *mut __m256i, db) };
            lane += 16;
        }

        while lane < count {
            let r = c0[lane];
            let g = c1[lane];
            let b = c2[lane];
            c0[lane] = g;
            c1[lane] = r.wrapping_sub(g);
            c2[lane] = b.wrapping_sub(g);
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            buf[base..base + 2].copy_from_slice(&c0[lane].to_le_bytes());
            buf[base + 2..base + 4].copy_from_slice(&c1[lane].to_le_bytes());
            buf[base + 4..base + 6].copy_from_slice(&c2[lane].to_le_bytes());
        }

        pixel_idx += count;
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn rgb16_delta_g_inverse_avx2(buf: &mut [u8]) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let pixels = buf.len() / 6;
    let block_pixels = rgb_planar_block_pixels(DEFAULT_RGB16_PLANAR_BLOCK_PIXELS);
    let mut c0 = vec![0u16; block_pixels];
    let mut c1 = vec![0u16; block_pixels];
    let mut c2 = vec![0u16; block_pixels];

    let mut pixel_idx = 0usize;
    while pixel_idx < pixels {
        let count = block_pixels.min(pixels - pixel_idx);
        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            c0[lane] = u16::from_le_bytes([buf[base], buf[base + 1]]);
            c1[lane] = u16::from_le_bytes([buf[base + 2], buf[base + 3]]);
            c2[lane] = u16::from_le_bytes([buf[base + 4], buf[base + 5]]);
        }

        let mut lane = 0usize;
        while lane + 16 <= count {
            let g = unsafe { _mm256_loadu_si256(c0.as_ptr().add(lane) as *const __m256i) };
            let dr = unsafe { _mm256_loadu_si256(c1.as_ptr().add(lane) as *const __m256i) };
            let db = unsafe { _mm256_loadu_si256(c2.as_ptr().add(lane) as *const __m256i) };
            let r = _mm256_add_epi16(g, dr);
            let b = _mm256_add_epi16(g, db);
            unsafe { _mm256_storeu_si256(c0.as_mut_ptr().add(lane) as *mut __m256i, r) };
            unsafe { _mm256_storeu_si256(c1.as_mut_ptr().add(lane) as *mut __m256i, g) };
            unsafe { _mm256_storeu_si256(c2.as_mut_ptr().add(lane) as *mut __m256i, b) };
            lane += 16;
        }

        while lane < count {
            let g = c0[lane];
            let dr = c1[lane];
            let db = c2[lane];
            c0[lane] = g.wrapping_add(dr);
            c1[lane] = g;
            c2[lane] = g.wrapping_add(db);
            lane += 1;
        }

        for lane in 0..count {
            let base = (pixel_idx + lane) * 6;
            buf[base..base + 2].copy_from_slice(&c0[lane].to_le_bytes());
            buf[base + 2..base + 4].copy_from_slice(&c1[lane].to_le_bytes());
            buf[base + 4..base + 6].copy_from_slice(&c2[lane].to_le_bytes());
        }

        pixel_idx += count;
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

fn read_tiff_metadata_for_sections(
    path: &Path,
) -> Result<(Option<f32>, Option<f32>, Option<Vec<u8>>)> {
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;

    let f = File::open(path)?;
    let mut dec = Decoder::new(std::io::BufReader::new(f))?;

    let unit = dec.get_tag(Tag::ResolutionUnit).ok();
    let xr = dec.get_tag(Tag::XResolution).ok();
    let yr = dec.get_tag(Tag::YResolution).ok();
    let icc = dec.get_tag_u8_vec(Tag::IccProfile).ok();

    let mut xdpi: Option<f32> = None;
    let mut ydpi: Option<f32> = None;

    if let (Some(unit_v), Some(xv), Some(yv)) = (unit, xr, yr) {
        let xf = rational_to_f32(&xv);
        let yf = rational_to_f32(&yv);
        if let Some(code) = short_first(&unit_v) {
            match code {
                2 => {
                    xdpi = Some(xf);
                    ydpi = Some(yf);
                }
                3 => {
                    xdpi = Some(xf * 2.54);
                    ydpi = Some(yf * 2.54);
                }
                _ => {}
            }
        }
    }

    Ok((xdpi, ydpi, icc))
}

fn parse_resolution_section(payload: &[u8]) -> Option<(f32, f32)> {
    if payload.len() != 8 {
        return None;
    }
    let x = f32::from_le_bytes(payload[0..4].try_into().ok()?);
    let y = f32::from_le_bytes(payload[4..8].try_into().ok()?);
    Some((x, y))
}

fn rational_to_f32(v: &tiff::decoder::ifd::Value) -> f32 {
    use tiff::decoder::ifd::Value::*;
    match v {
        Rational(n, d) => {
            if *d == 0 {
                0.0
            } else {
                *n as f32 / *d as f32
            }
        }
        SRational(n, d) => {
            if *d == 0 {
                0.0
            } else {
                *n as f32 / *d as f32
            }
        }
        Short(s) => *s as f32,
        Float(f) => *f,
        Double(f) => *f as f32,
        _ => 0.0,
    }
}

fn short_first(v: &tiff::decoder::ifd::Value) -> Option<u32> {
    use tiff::decoder::ifd::Value::*;
    match v {
        Short(s) => Some(*s as u32),
        _ => None,
    }
}

fn bytes_to_u16_vec(data: &[u8]) -> Result<Vec<u16>> {
    ensure!(data.len() % 2 == 0, "16-bit data must have even byte length");
    Ok(data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect())
}

fn u16_vec_to_bytes(data: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for value in data {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16_le<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32_le<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64_le<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_rgb8() -> TiffImage {
        TiffImage {
            width: 4,
            height: 4,
            color_type: ColorType::Rgb8,
            data: vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120,
                11, 21, 31, 41, 51, 61, 71, 81, 91, 101, 111, 121,
                12, 22, 32, 42, 52, 62, 72, 82, 92, 102, 112, 122,
                13, 23, 33, 43, 53, 63, 73, 83, 93, 103, 113, 123,
            ],
            xdpi: Some(300.0),
            ydpi: Some(300.0),
            icc: Some(vec![1, 2, 3, 4]),
        }
    }

    fn sample_rgb16() -> TiffImage {
        let mut data = Vec::new();
        for y in 0..4u16 {
            for x in 0..4u16 {
                data.extend_from_slice(&(1000 + x * 17 + y * 5).to_le_bytes());
                data.extend_from_slice(&(2000 + x * 13 + y * 7).to_le_bytes());
                data.extend_from_slice(&(3000 + x * 11 + y * 9).to_le_bytes());
            }
        }
        TiffImage {
            width: 4,
            height: 4,
            color_type: ColorType::Rgb16,
            data,
            xdpi: None,
            ydpi: None,
            icc: None,
        }
    }

    #[test]
    fn roundtrip_sections_and_pyramid_work() {
        let cfg = CTIConfig {
            tile_size: 2,
            compression: CompressionType::Zstd,
            color_transform: true,
            pyramid_levels: 2,
            ..CTIConfig::default()
        };
        let enc = CTIEncoder::new(cfg);
        let img = sample_rgb8();
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        cur.set_position(0);
        let info = CTIDecoder::info_from_reader(&mut cur).unwrap();
        assert_eq!(info.header.width, 4);
        assert_eq!(info.pyramid_levels.len(), 2);
        assert_eq!(info.icc_size, Some(4));
        assert_eq!(info.xdpi, Some(300.0));

        cur.set_position(0);
        let decoded = CTIDecoder::decode_from_reader(&mut cur, 0).unwrap();
        assert_eq!(decoded.data, img.data);

        cur.set_position(0);
        let level1 = CTIDecoder::decode_from_reader(&mut cur, 1).unwrap();
        assert_eq!(level1.header.width, 2);
        assert_eq!(level1.header.height, 2);
    }

    #[test]
    fn tile_and_region_decode_work() {
        let img = TiffImage {
            width: 4,
            height: 4,
            color_type: ColorType::L8,
            data: (0u8..16).collect(),
            xdpi: None,
            ydpi: None,
            icc: None,
        };
        let enc = CTIEncoder::new(CTIConfig {
            tile_size: 2,
            compression: CompressionType::Lz4,
            ..CTIConfig::default()
        });
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        cur.set_position(0);
        let tile = CTIDecoder::decode_tile_from_reader(&mut cur, 1, 0, 0).unwrap();
        assert_eq!(tile.width, 2);
        assert_eq!(tile.height, 2);
        assert_eq!(tile.data, vec![2, 3, 6, 7]);

        cur.set_position(0);
        let region = CTIDecoder::extract_region_from_reader(&mut cur, 1, 1, 2, 2, 0).unwrap();
        assert_eq!(region.data, vec![5, 6, 9, 10]);
    }

    #[test]
    fn corruption_is_detected() {
        let enc = CTIEncoder::new(CTIConfig::default());
        let img = sample_rgb8();
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        let mut bytes = cur.into_inner();
        let mut info_cur = Cursor::new(bytes.clone());
        let info = CTIDecoder::info_from_reader(&mut info_cur).unwrap();
        let off = info.indices[0].offset as usize;
        bytes[off] ^= 0x5A;

        let mut bad_cur = Cursor::new(bytes);
        assert!(CTIDecoder::decode_from_reader(&mut bad_cur, 0).is_err());
    }

    #[test]
    fn adaptive_rgb16_roundtrip_uses_tile_modes() {
        let enc = CTIEncoder::new(CTIConfig {
            tile_size: 2,
            compression: CompressionType::Adaptive,
            ..CTIConfig::default()
        });
        let img = sample_rgb16();
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        cur.set_position(0);
        let decoded = CTIDecoder::decode_from_reader(&mut cur, 0).unwrap();
        assert_eq!(decoded.data, img.data);

        cur.set_position(0);
        let sections = CTIDecoder::sections_from_reader(&mut cur).unwrap();
        let tmod = sections.iter().find(|sec| sec.ty == SEC_TYPE_TMOD).unwrap();
        assert_eq!(tmod.payload.len(), 4);
    }

    #[test]
    fn web_storage_downcasts_rgb16_to_rgb8() {
        let img = sample_rgb16();
        let expected = downcast_u16_bytes_to_u8(&img.data).unwrap();
        let enc = CTIEncoder::new(CTIConfig {
            tile_size: 2,
            compression: CompressionType::Lz4,
            downcast_16_to_8: true,
            ..CTIConfig::default()
        });
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        cur.set_position(0);
        let info = CTIDecoder::info_from_reader(&mut cur).unwrap();
        assert_eq!(info.header.color_type, 3);

        cur.set_position(0);
        let decoded = CTIDecoder::decode_from_reader(&mut cur, 0).unwrap();
        assert_eq!(decoded.data, expected);
    }

    #[test]
    fn shuffle16_roundtrip_works() {
        let img = sample_rgb16();
        let shuffled = shuffle_u16_bytes(&img.data).unwrap();
        let restored = unshuffle_u16_bytes(&shuffled).unwrap();
        assert_eq!(restored, img.data);
    }

    #[test]
    fn gradient16_roundtrip_works() {
        let img = sample_rgb16();
        let gradient = gradient_forward_u16(&img.data, img.width as usize, 3).unwrap();
        let restored = gradient_inverse_u16(&gradient, img.width as usize, 3).unwrap();
        assert_eq!(restored, img.data);
    }

    #[test]
    fn rgb16_delta_g_roundtrip_handles_extremes() {
        let mut data = Vec::new();
        let samples = [
            (0u16, 65535u16, 1u16),
            (65535u16, 0u16, 65534u16),
            (32768u16, 32767u16, 0u16),
            (1u16, 2u16, 65535u16),
        ];
        for (r, g, b) in samples {
            data.extend_from_slice(&r.to_le_bytes());
            data.extend_from_slice(&g.to_le_bytes());
            data.extend_from_slice(&b.to_le_bytes());
        }
        let original = data.clone();
        rgb16_delta_g_forward(&mut data);
        rgb16_delta_g_inverse(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn rgb16_rct_roundtrip_handles_sample_data() {
        let mut data = sample_rgb16().data;
        let original = data.clone();
        rct_forward_rgb16(&mut data);
        rct_inverse_rgb16(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn rgb16_color_transform_roundtrip_works() {
        let enc = CTIEncoder::new(CTIConfig {
            tile_size: 2,
            compression: CompressionType::Adaptive,
            color_transform: true,
            ..CTIConfig::default()
        });
        let img = sample_rgb16();
        let mut cur = Cursor::new(Vec::new());
        enc.encode_to_writer(&img, &mut cur).unwrap();

        cur.set_position(0);
        let info = CTIDecoder::info_from_reader(&mut cur).unwrap();
        assert_eq!(info.header.flags & FLAG_COLOR_RGB16_DELTA_G, FLAG_COLOR_RGB16_DELTA_G);

        cur.set_position(0);
        let decoded = CTIDecoder::decode_from_reader(&mut cur, 0).unwrap();
        assert_eq!(decoded.data, img.data);
    }
}
