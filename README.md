# cti

Custom Tiled Images

---

## Build from Source

### Prerequisites
1. install [Git](https://git-scm.com/)
2. install [**Rust** (stable)](https://www.rust-lang.org/tools/install) and Cargo

### Compilation   
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
## Quickstart
```bash
# Encoding, NDK preset
.\cti.exe encode in.tif out.cti --ndk
```
```bash
# Decoding: RAW file and PNG Preview (8bpc)
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
