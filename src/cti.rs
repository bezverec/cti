use anyhow::{anyhow, bail, ensure, Context, Result};
use image::{codecs::tiff::TiffDecoder, ColorType, GenericImageView, ImageDecoder};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

// ====== Layout & konstanty ======
pub const CTI_MAGIC: &[u8; 4] = b"CTI1";
pub const CTI_HEADER_SIZE: usize = 64;
pub const TILE_INDEX_ONDISK_SIZE: usize = 8 + 4 + 4 + 4;
pub const DEFAULT_TILE_SIZE: u32 = 256;

const TAG_ESCAPE_FF: u8 = 0x00;
const TAG_RLE: u8 = 0x01;
const TAG_LZ77: u8 = 0x02;

// ====== Konfigurace ======
#[derive(Debug, Clone)]
pub struct CTIConfig {
    pub tile_size: u32,
    pub compression: CompressionType,
    pub quality_level: u8,
    pub color_transform: bool,
    pub zstd_level: i32,
}
impl Default for CTIConfig {
    fn default() -> Self {
        Self {
            tile_size: DEFAULT_TILE_SIZE,
            compression: CompressionType::Zstd,
            quality_level: 100,
            color_transform: false,
            zstd_level: 6,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum CompressionType {
    None = 0,
    RLE = 1,
    LZ77 = 2,
    Delta = 3,
    Predictive = 4,
    Zstd = 10,
    Lz4 = 11,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
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

#[derive(Debug, Clone, Copy)]
pub struct TileIndex {
    pub offset: u64,
    pub compressed_size: u32,
    pub original_size: u32,
    pub crc32: u32,
}

#[derive(Debug)]
pub struct TiffImage {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub data: Vec<u8>,
    pub xdpi: Option<f32>,
    pub ydpi: Option<f32>,
    pub icc: Option<Vec<u8>>,
}

// ====== Enkodér / Dekodér ======
pub struct CTIEncoder {
    config: CTIConfig,
}
impl CTIEncoder {
    pub fn new(config: CTIConfig) -> Self {
        Self { config }
    }

    pub fn load_tiff<P: AsRef<Path>>(&self, path: P) -> Result<TiffImage> {
        let p = path.as_ref();
        let file = File::open(p).with_context(|| format!("open {:?}", p))?;
        let mut br = BufReader::new(file);

        let (width, height, color_type, data) = match TiffDecoder::new(&mut br) {
            Ok(d) => {
                let (w, h) = d.dimensions();
                let ct = d.color_type();
                let mut buf = vec![0u8; d.total_bytes() as usize];
                d.read_image(&mut buf)?;
                (w, h, ct, buf)
            }
            Err(_) => {
                let dynimg =
                    image::open(p).with_context(|| format!("image::open fallback for {:?}", p))?;
                let (w, h) = dynimg.dimensions();
                let rgb = dynimg.to_rgb8();
                (w, h, ColorType::Rgb8, rgb.into_raw())
            }
        };

        let (xdpi, ydpi, icc) = read_tiff_metadata_for_sections(p).unwrap_or((None, None, None));

        Ok(TiffImage {
            width,
            height,
            color_type,
            data,
            xdpi,
            ydpi,
            icc,
        })
    }

    pub fn encode_to_cti<P: AsRef<Path>>(&self, img: &TiffImage, out_path: P) -> Result<()> {
        let mut bw = BufWriter::new(File::create(out_path.as_ref())?);

        let tiles_x = (img.width + self.config.tile_size - 1) / self.config.tile_size;
        let tiles_y = (img.height + self.config.tile_size - 1) / self.config.tile_size;
        let total_tiles = (tiles_x * tiles_y) as usize;

        let color_type_id = match img.color_type {
            ColorType::L8 => 1,
            ColorType::L16 => 2,
            ColorType::Rgb8 => 3,
            ColorType::Rgba8 => 4,
            ColorType::Rgb16 => 5,
            _ => bail!("Unsupported color type: {:?}", img.color_type),
        };

        let mut flags: u16 = 0;
        if self.config.color_transform {
            flags |= 1;
        }

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
        write_header(&mut bw, &header)?;

        // index předalokovat (přeskočit), data pak hned za ním
        let index_offset = CTI_HEADER_SIZE as u64;
        let index_size = total_tiles * TILE_INDEX_ONDISK_SIZE;
        let data_offset = index_offset + index_size as u64;
        bw.seek(SeekFrom::Start(data_offset))?;

        #[derive(Clone)]
        struct CompTile {
            comp: Vec<u8>,
            orig_len: u32,
            crc: u32,
        }

        let use_rct =
            self.config.color_transform && matches!(img.color_type, ColorType::Rgb8 | ColorType::Rgb16);
        let zstd_level = self.config.zstd_level;

        // Paralelní komprese – uloženo rovnou podle lineárního indexu (bez O(T^2) vyhledávání)
        let comp_tiles: Vec<CompTile> = (0..total_tiles)
            .into_par_iter()
            .map(|idx| -> Result<CompTile> {
                let tx = (idx as u32) % tiles_x;
                let ty = (idx as u32) / tiles_x;

                let mut tile = extract_tile(img, tx, ty, self.config.tile_size)?;
                if use_rct {
                    match img.color_type {
                        ColorType::Rgb8 => rct_forward_rgb8(&mut tile),
                        ColorType::Rgb16 => rct_forward_rgb16(&mut tile),
                        _ => {}
                    }
                }

                // 16bit vstupy vynutíme na Zstd (ostatní varianty necháme)
                let use_kind = match img.color_type {
                    ColorType::L16 | ColorType::Rgb16 => match self.config.compression {
                        CompressionType::None
                        | CompressionType::RLE
                        | CompressionType::LZ77
                        | CompressionType::Delta
                        | CompressionType::Predictive => CompressionType::Zstd,
                        k => k,
                    },
                    _ => self.config.compression,
                };

                let comp = compress_tile(use_kind, &tile, zstd_level)?;
                Ok(CompTile {
                    comp,
                    orig_len: tile.len() as u32,
                    crc: crc32(&tile),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // Zápis dat + indexů v přirozeném pořadí
        let mut indices: Vec<TileIndex> = Vec::with_capacity(total_tiles);
        let mut cursor = data_offset;

        for idx in 0..total_tiles {
            let ct = &comp_tiles[idx];
            bw.write_all(&ct.comp)?;
            indices.push(TileIndex {
                offset: cursor,
                compressed_size: ct.comp.len() as u32,
                original_size: ct.orig_len,
                crc32: ct.crc,
            });
            cursor += ct.comp.len() as u64;
        }

        // zapiš indexový blok
        bw.seek(SeekFrom::Start(index_offset))?;
        for idx in &indices {
            write_tile_index(&mut bw, idx)?;
        }

        // sekce (DPI, ICC)
        bw.seek(SeekFrom::Start(cursor))?;
        let mut sections: Vec<(u32, Vec<u8>)> = Vec::new();
        if let (Some(x), Some(y)) = (img.xdpi, img.ydpi) {
            let mut res = Vec::with_capacity(8);
            res.extend_from_slice(&x.to_le_bytes());
            res.extend_from_slice(&y.to_le_bytes());
            sections.push((SEC_TYPE_RES, res));
        }
        if let Some(icc) = &img.icc {
            sections.push((SEC_TYPE_ICC, icc.clone()));
        }
        write_sections(&mut bw, &sections)?;
        bw.flush()?;
        Ok(())
    }
}

pub struct CTIDecoder;
impl CTIDecoder {
    pub fn info<P: AsRef<Path>>(path: P) -> Result<CTIHeader> {
        let mut br = BufReader::new(File::open(path)?);
        let hdr = read_header(&mut br)?;
        ensure!(&hdr.magic == CTI_MAGIC, "Bad magic");
        Ok(hdr)
    }

    pub fn decode<P: AsRef<Path>>(path: P) -> Result<(CTIHeader, Vec<u8>)> {
        let mut f = BufReader::new(File::open(path.as_ref())?);
        let hdr = read_header(&mut f)?;
        ensure!(&hdr.magic == CTI_MAGIC, "Bad magic");

        let total_tiles = (hdr.tiles_x * hdr.tiles_y) as usize;
        let indices = read_indices(&mut f, total_tiles)?;

        let bpp = match hdr.color_type {
            1 => 1u32,
            2 => 2u32,
            3 => 3u32,
            4 => 4u32,
            5 => 6u32,
            _ => bail!("Unsupported color type id {}", hdr.color_type),
        };

        let mut out = vec![0u8; (hdr.width * hdr.height * bpp) as usize];
        let use_rct = (hdr.flags & 1) != 0 && matches!(hdr.color_type, 3 | 5);

        let mut file = f.into_inner();
        for (i, t) in indices.iter().enumerate() {
            file.seek(SeekFrom::Start(t.offset))?;
            let mut comp = vec![0u8; t.compressed_size as usize];
            file.read_exact(&mut comp)?;

            let mut tile_bytes =
                decompress_tile_with_size(hdr.compression, &comp, t.original_size as usize)?;
            ensure!(crc32(&tile_bytes) == t.crc32, "CRC mismatch at tile {}", i);

            if use_rct {
                match hdr.color_type {
                    3 => rct_inverse_rgb8(&mut tile_bytes),
                    5 => rct_inverse_rgb16(&mut tile_bytes),
                    _ => {}
                }
            }
            let tx = (i as u32) % hdr.tiles_x;
            let ty = (i as u32) / hdr.tiles_x;
            blit_tile(
                &mut out,
                &tile_bytes,
                hdr.width,
                hdr.height,
                hdr.tile_size,
                bpp as u32,
                tx,
                ty,
            )?;
        }
        Ok((hdr, out))
    }
}

// ====== I/O helpery ======
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

// ====== Tile operace ======
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
fn extract_tile(img: &TiffImage, tx: u32, ty: u32, ts: u32) -> Result<Vec<u8>> {
    let bpp = bytes_per_pixel(&img.color_type)?;
    let start_x = tx * ts;
    let start_y = ty * ts;
    let end_x = (start_x + ts).min(img.width);
    let end_y = (start_y + ts).min(img.height);
    let tile_w = end_x - start_x;
    let tile_h = end_y - start_y;

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

// ====== Komprese / dekomprese ======
fn compress_tile(kind: CompressionType, data: &[u8], zstd_level: i32) -> Result<Vec<u8>> {
    Ok(match kind {
        CompressionType::None => data.to_vec(),
        CompressionType::RLE => rle_compress(data)?,
        CompressionType::Delta => rle_compress(&delta_forward(data))?,
        CompressionType::Predictive => rle_compress(&predictive_forward(data))?,
        CompressionType::LZ77 => lz77_compress(data)?,
        CompressionType::Zstd => zstd::bulk::compress(data, zstd_level)?,
        CompressionType::Lz4 => lz4_flex::block::compress_prepend_size(data),
    })
}
fn decompress_tile_with_size(kind: u8, comp: &[u8], original_size: usize) -> Result<Vec<u8>> {
    match kind {
        0 => Ok(comp.to_vec()),
        1 => rle_decompress(comp),
        2 => lz77_decompress(comp),
        3 => {
            let d = rle_decompress(comp)?;
            Ok(delta_inverse(&d))
        }
        4 => {
            let d = rle_decompress(comp)?;
            Ok(predictive_inverse(&d))
        }
        10 => zstd::bulk::decompress(comp, original_size)
            .map_err(|e| anyhow!("zstd decompress failed: {e}")),
        11 => lz4_flex::block::decompress_size_prepended(comp).map_err(|e| anyhow!(e)),
        _ => bail!("Unknown compression id {}", kind),
    }
}

// ===== RLE =====
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
                out.extend(std::iter::repeat(val).take(count));
            }
            TAG_LZ77 => bail!("RLE stream contains LZ77 tag"),
            _ => bail!("RLE unknown tag {}", tag),
        }
    }
    Ok(out)
}

// ===== Delta/predictive =====
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
fn delta_inverse(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![];
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = data[0];
    out.push(prev);
    for i in 1..data.len() {
        let v = prev.wrapping_add(data[i]);
        out.push(v);
        prev = v;
    }
    out
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
fn predictive_inverse(data: &[u8]) -> Vec<u8> {
    if data.len() < 3 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut a0, mut a1) = (data[0], data[1]);
    out.push(a0);
    out.push(a1);
    for i in 2..data.len() {
        let p = a1.wrapping_add(a1.wrapping_sub(a0));
        let v = p.wrapping_add(data[i]);
        out.push(v);
        a0 = a1;
        a1 = v;
    }
    out
}

// ===== LZ77 (demo) =====
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
                out.extend(std::iter::repeat(val).take(count));
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

// ===== RCT 5/3-like (scalar) =====
fn rct_forward_rgb8(buf: &mut [u8]) {
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
    for p in buf.chunks_exact_mut(3) {
        let y = p[0] as i32;
        let cb = (p[1] as i8) as i32; // signed
        let cr = (p[2] as i8) as i32; // signed
        let g = y - ((cb + cr) >> 2);
        let r = cr + g;
        let b = cb + g;
        p[0] = r.clamp(0, 255) as u8;
        p[1] = g.clamp(0, 255) as u8;
        p[2] = b.clamp(0, 255) as u8;
    }
}
fn rct_forward_rgb16(buf: &mut [u8]) {
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

// ===== CRC32 (rychlé) =====
pub fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

// ===== Sections (TOC at end) =====
pub const SEC_TYPE_RES: u32 = 0x2053_4552; // 'RES '
pub const SEC_TYPE_ICC: u32 = 0x2043_4349; // 'ICC '
pub struct SectionDesc {
    pub ty: u32,
    pub offset: u64,
    pub size: u64,
}
pub fn write_sections<W: Write + Seek>(w: &mut W, sections: &[(u32, Vec<u8>)]) -> Result<()> {
    if sections.is_empty() {
        w.write_all(&0u32.to_le_bytes())?;
        return Ok(());
    }
    let count = sections.len() as u32;
    w.write_all(&count.to_le_bytes())?;
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

// ===== TIFF metadata helper (DPI, ICC optional – ICC ponecháno None kvůli tiff 0.10) =====
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

    let icc_bytes: Option<Vec<u8>> = None;
    Ok((xdpi, ydpi, icc_bytes))
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
