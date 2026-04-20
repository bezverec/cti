# cti

![version](https://img.shields.io/badge/dynamic/toml?url=https://raw.githubusercontent.com/bezverec/cti/main/Cargo.toml&query=$.package.version&label=version&prefix=v) ![GitHub top language](https://img.shields.io/github/languages/top/bezverec/cti) ![GitHub last commit](https://img.shields.io/github/last-commit/bezverec/cti) ![GitHub commit activity](https://img.shields.io/github/commit-activity/m/bezverec/cti) ![GitHub repo size](https://img.shields.io/github/repo-size/bezverec/cti) ![LoC](https://tokei.rs/b1/github/bezverec/cti) 
![Dependencies](https://deps.rs/repo/github/bezverec/cti/status.svg)

Custom Tiled Image is an experimental tiled still-image file format, encoder and decoder with support for image -> CTI conversion, metadata sections, random tile/region access and optional embedded pyramid levels.

---

## Build from Source

### Prerequisites
1. install [Git](https://git-scm.com/)
2. install [**Rust** (stable)](https://www.rust-lang.org/tools/install) and Cargo

### Compilation (Windows)   
1. ```bash
   git clone https://github.com/bezverec/cti.git
   ```
2. ```bash
   cd cti
   ```
3. ```bash
   $env:RUSTFLAGS="-C target-cpu=native"; cargo build --release
   # binary will be in: .\target\release\cti.exe
   ```
---
## Quickstart (Windows)
```bash
# Encoding with NDK preset (tile=4096, adaptive lossless)
.\cti.exe encode in.tif out.cti --ndk
```
```bash
# Encoding with explicit compression and 3 pyramid levels
.\cti.exe encode in.png out.cti --compression zstd --tile 512 --pyramid-levels 3
```
```bash
# Named presets
.\cti.exe encode in.tif out-archive.cti --preset archive
.\cti.exe encode in.tif out-web.cti --preset web
.\cti.exe encode in.tif out-web-zstd.cti --preset web-zstd
```
```bash
# Decoding full image to RAW + PNG/TIFF by extension
.\cti.exe decode out.cti out.raw --image-out out.png
.\cti.exe decode out.cti out.raw --image-out out.tif
```
```bash
# Decode one tile only
.\cti.exe decode-tile out.cti 2 1 tile.raw --image-out tile.png
```
```bash
# Decode one region only
.\cti.exe extract-region out.cti 1024 2048 512 512 region.raw --image-out region.png
```
```bash
# Info and section dump
.\cti.exe info out.cti
.\cti.exe dump-sections out.cti
```
## Benchmark
```bash
# encode benchmark (NDK preset)
.\cti.exe bench encode in.tif --ndk --repeat 3
```
```bash
# decode benchmark, optionally on a pyramid level
.\cti.exe bench decode out.cti --repeat 5
.\cti.exe bench decode out.cti --level 1 --repeat 5
```
---

## Current capabilities

- Input loading from TIFF and common raster formats supported by the `image` crate.
- Compression backends: None, RLE, LZ77, Delta+RLE, Predictive+RLE, Zstd, LZ4 and adaptive per-tile lossless mode.
- Optional reversible color decorrelation for RGB: classic RCT for `RGB8`, exact `DeltaG` lifting for `RGB16`.
- 16-bit aware delta, predictive, byte-shuffle and gradient transforms with AVX2-assisted adaptive tile scoring on x86/x86_64.
- Preset profiles: `archive` for smaller lossless output, `web` for fastest distribution, `web-zstd` for smaller 8-bit web payloads.
- End-to-end metadata sections for DPI and ICC profiles.
- Partial decode APIs and CLI commands for individual tiles and arbitrary regions.
- Optional embedded pyramid levels stored as CTI payload sections.
- Image export from decode paths using file extension (`.png`, `.tif`, ...), including 16-bit grayscale and RGB outputs.
- Optional tuning hooks: `CTI_BATCH_TILES=<N>` overrides streaming/pyramid compression batching, `CTI_RGB_PLANAR_BLOCK_PIXELS=<N>` overrides RGB planar scratch block size for AVX2 RGB experiments.

---
## AI generated code disclosure

The code is AI generated using ChatGPT model 5.

---
## Viewer

A simple viewer can be found here: https://github.com/bezverec/cti-view

---

# 📄 CTI File Format Specification – Version 1.1

---

## 🇨🇿 Česká verze

### 1. Účel a motivace
Formát **CTI (Custom Tiled Image)** je navržen pro archivní a technické použití, kde je klíčová:
- dlaždicová organizace obrazu,
- podpora bezeztrátových i ztrátových kompresí,
- rychlé dekódování a paralelní přístup k částem obrazu,
- rozšiřitelnost pomocí volitelných metadatových sekcí.

---

### 2. Celková struktura souboru

```
+-------------------+
| Hlavička (Header) |   pevná délka 64 B
+-------------------+
| Tabulka indexů    |   záznamy o dlaždicích
+-------------------+
| Data dlaždic      |   komprimovaná data jednotlivých dlaždic
+-------------------+
| Sekce (Sections)  |   volitelná metadata (DPI, ICC, …)
+-------------------+
```

---

### 3. Hlavička (CTI Header)

Velikost: **64 bajtů**, little-endian.

| Pole         | Typ     | Velikost | Popis |
|--------------|---------|----------|-------|
| `magic`      | u8[4]   | 4 B      | Signatura `"CTI1"` |
| `version`    | u16     | 2 B      | Major verze formátu (`1`), specifikace tohoto dokumentu je 1.1 |
| `flags`      | u16     | 2 B      | Bitové příznaky (viz níže) |
| `width`      | u32     | 4 B      | Šířka obrazu (px) |
| `height`     | u32     | 4 B      | Výška obrazu (px) |
| `tile_size`  | u32     | 4 B      | Velikost dlaždice (px) |
| `tiles_x`    | u32     | 4 B      | Počet dlaždic horizontálně |
| `tiles_y`    | u32     | 4 B      | Počet dlaždic vertikálně |
| `color_type` | u8      | 1 B      | Typ barev (viz tabulka) |
| `compression`| u8      | 1 B      | ID komprese |
| `quality`    | u8      | 1 B      | Kvalita (0–100, dle komprese) |
| `reserved`   | u8[33]  | 33 B     | Rezerva |

**ColorType IDs**

| ID | Název  | Popis |
|----|--------|-------|
| 1  | L8     | 8-bit grayscale |
| 2  | L16    | 16-bit grayscale |
| 3  | RGB8   | 24-bit |
| 4  | RGBA8  | 32-bit |
| 5  | RGB16  | 48-bit |

**Compression IDs**

| ID  | Název          | Popis |
|-----|----------------|-------|
| 0   | None           | nekomprimovaná data |
| 1   | RLE            | run-length encoding |
| 2   | LZ77           | slovníková komprese |
| 3   | Delta+RLE      | rozdílové kódování + RLE |
| 4   | Predictive+RLE | prediktor + RLE |
| 10  | Zstd           | Zstandard |
| 11  | LZ4            | LZ4 block |
| 250 | Adaptive       | per-tile volba lossless módu, metadata v `TMOD` |

**Flags**
- Bit 0: legacy RCT zapnuto
- Bit 1: `RGB16DeltaG` decorrelace zapnuta
- Ostatní bity rezervovány

---

### 4. Tabulka indexů

Počet záznamů = `tiles_x * tiles_y`, velikost záznamu **20 B**.

| Pole              | Typ   | Velikost | Popis |
|-------------------|-------|----------|-------|
| `offset`          | u64   | 8 B      | Offset do dat |
| `compressed_size` | u32   | 4 B      | Velikost komprimovaných dat |
| `original_size`   | u32   | 4 B      | Velikost původních dat |
| `crc32`           | u32   | 4 B      | CRC32 původních dat |

---

### 5. Data dlaždic
- Uložena sekvenčně dle tabulky indexů.  
- Komprimace dle `compression` v hlavičce.  
- Při `compression = 250 (Adaptive)` se konkrétní lossless mód každé dlaždice bere ze sekce `TMOD`.

---

### 6. Sekce (metadata)
Na konci souboru může být TOC a payloady.

```
u32 count
[count × (ty: u32, offset: u64, size: u64)]
payloads...
```

**Typy sekcí**
| Typ (u32) | ASCII | Popis |
|-----------|-------|-------|
| 0x2053_4552 | "RES " | Rozlišení DPI (2× f32) |
| 0x2043_4349 | "ICC " | ICC profil |
| 0x564C_5950 | "PYLV" | Vnořená CTI pyramid level payload |
| 0x444F_4D54 | "TMOD" | 1 byte na dlaždici: zvolený adaptivní mód (`0=ZstdRaw`, `1=Delta16`, `2=Predict16`, `3=Shuffle16`, `4=Gradient16`, `5=Lz4Raw`) |

---

### 7. Integrita
- **CRC32** každé dlaždice.  
- Hlavička obsahuje `magic` a `version`.

---

### 8. Binární struktura (C-like)

```c
struct CTIHeader {
    char magic[4];       // "CTI1"
    uint16_t version;    // = 1
    uint16_t flags;
    uint32_t width;
    uint32_t height;
    uint32_t tile_size;
    uint32_t tiles_x;
    uint32_t tiles_y;
    uint8_t color_type;
    uint8_t compression;
    uint8_t quality;
    uint8_t reserved[33];
};

struct TileIndex {
    uint64_t offset;
    uint32_t compressed_size;
    uint32_t original_size;
    uint32_t crc32;
};
```

---

## 🇬🇧 English version

### 1. Purpose and motivation
The **CTI (Custom Tiled Image)** format is designed for archival and technical applications where:
- tile-based organization is required,
- both lossless and lossy compression are supported,
- fast decoding and parallel random access to tiles is important,
- extensibility via optional metadata sections is needed.

---

### 2. Overall file structure

```
+-------------------+
| Header            |   fixed size 64 B
+-------------------+
| Tile Index Table  |   per-tile records
+-------------------+
| Tile Data         |   compressed tiles
+-------------------+
| Sections (TOC)    |   optional metadata (DPI, ICC, …)
+-------------------+
```

---

### 3. Header

Fixed size: **64 bytes**, little-endian.

| Field        | Type   | Size | Description |
|--------------|--------|------|-------------|
| `magic`      | u8[4]  | 4 B  | Signature `"CTI1"` |
| `version`    | u16    | 2 B  | Major format version (`1`), while this document describes spec revision 1.1 |
| `flags`      | u16    | 2 B  | Bit flags |
| `width`      | u32    | 4 B  | Image width (px) |
| `height`     | u32    | 4 B  | Image height (px) |
| `tile_size`  | u32    | 4 B  | Tile size (px) |
| `tiles_x`    | u32    | 4 B  | Tiles horizontally |
| `tiles_y`    | u32    | 4 B  | Tiles vertically |
| `color_type` | u8     | 1 B  | Color type ID |
| `compression`| u8     | 1 B  | Compression ID |
| `quality`    | u8     | 1 B  | Quality (0–100) |
| `reserved`   | u8[33] | 33 B | Reserved |

**ColorType IDs**

| ID | Name  | Description |
|----|-------|-------------|
| 1  | L8    | 8-bit grayscale |
| 2  | L16   | 16-bit grayscale |
| 3  | RGB8  | 24-bit RGB |
| 4  | RGBA8 | 32-bit RGBA |
| 5  | RGB16 | 48-bit RGB |

**Compression IDs**

| ID  | Name            | Description |
|-----|-----------------|-------------|
| 0   | None            | uncompressed |
| 1   | RLE             | run-length encoding |
| 2   | LZ77            | simple dictionary |
| 3   | Delta+RLE       | delta coding + RLE |
| 4   | Predictive+RLE  | 2nd-order predictor + RLE |
| 10  | Zstd            | Zstandard |
| 11  | LZ4             | LZ4 block |
| 250 | Adaptive        | per-tile lossless mode selection, payload described by `TMOD` |

**Flags**
- Bit 0 = legacy RCT enabled  
- Bit 1 = `RGB16DeltaG` decorrelation enabled  
- Others reserved  

---

### 4. Tile Index Table

Number of entries = `tiles_x * tiles_y`.  
Each entry = **20 bytes**.

| Field            | Type   | Size | Description |
|------------------|--------|------|-------------|
| `offset`         | u64    | 8 B  | Tile data offset |
| `compressed_size`| u32    | 4 B  | Size of compressed data |
| `original_size`  | u32    | 4 B  | Size of uncompressed data |
| `crc32`          | u32    | 4 B  | CRC32 of uncompressed data |

---

### 5. Tile Data
- Stored sequentially as per index table.  
- Compressed with method in `compression` field.
- When `compression = 250 (Adaptive)`, the concrete per-tile lossless backend is stored in the `TMOD` section.

---

### 6. Sections (optional metadata)

At the end of the file:

```
u32 count
[count × (ty: u32, offset: u64, size: u64)]
payloads...
```

**Section types**

| Type (u32) | ASCII | Description |
|------------|-------|-------------|
| 0x2053_4552 | "RES " | DPI (2× f32: X, Y) |
| 0x2043_4349 | "ICC " | ICC profile (binary blob) |
| 0x564C_5950 | "PYLV" | Embedded CTI pyramid level payload |
| 0x444F_4D54 | "TMOD" | 1 byte per tile with adaptive mode id (`0=ZstdRaw`, `1=Delta16`, `2=Predict16`, `3=Shuffle16`, `4=Gradient16`, `5=Lz4Raw`) |

---

### 7. Integrity
- Each tile validated by **CRC32**.  
- Header has magic `"CTI1"` and version field.

---

### 8. Binary structure (C-like)

```c
struct CTIHeader {
    char magic[4];       // "CTI1"
    uint16_t version;    // = 1
    uint16_t flags;
    uint32_t width;
    uint32_t height;
    uint32_t tile_size;
    uint32_t tiles_x;
    uint32_t tiles_y;
    uint8_t color_type;
    uint8_t compression;
    uint8_t quality;
    uint8_t reserved[33];
};

struct TileIndex {
    uint64_t offset;
    uint32_t compressed_size;
    uint32_t original_size;
    uint32_t crc32;
};
```

