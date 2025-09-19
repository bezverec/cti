# cti

![version](https://img.shields.io/badge/dynamic/toml?url=https://raw.githubusercontent.com/bezverec/cti/main/Cargo.toml&query=$.package.version&label=version&prefix=v) ![GitHub top language](https://img.shields.io/github/languages/top/bezverec/cti) ![GitHub last commit](https://img.shields.io/github/last-commit/bezverec/cti) ![GitHub commit activity](https://img.shields.io/github/commit-activity/m/bezverec/cti) ![GitHub repo size](https://img.shields.io/github/repo-size/bezverec/cti) ![LoC](https://tokei.rs/b1/github/bezverec/cti) 
![Dependencies](https://deps.rs/repo/github/bezverec/cti/status.svg)

Custom Tiled Image is an experimental still image file format and encoder from TIFF to CTI.

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
   # binary will be in: .\cti\target\release\cti.exe
   ```
---
## Quickstart (Windows)
```bash
# Encoding, NDK preset
.\cti.exe encode in.tif out.cti --ndk
```
```bash
# Decoding: CTI file, RAW file and PNG file (8bpc)
.\cti.exe decode out.cti out.raw --png-out out.png
```
```bash
# Info
.\cti.exe info out.cti
```
## Benchmark
```bash
# encode bench benchmark (NDK preset)
.\cti.exe bench encode in.tif --ndk --repeat 3
```
```bash
# decode benchmark
.\cti.exe bench decode out.cti --repeat 5
```
---
## AI generated code disclosure

The code is AI generated using ChatGPT model 5.
