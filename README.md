# cti
Custom Tiled Images

## Build from source
```bash
$env:RUSTFLAGS="-C target-cpu=native"; cargo build --release
```
## Quickstart
```bash
# Encoding, NDK preset
./cti.exe encode in.tif out.cti --ndk
```
```bash
# Decoding: RAW file and PNG Preview (8bpc)
./cti.exe decode out.cti out.raw --png-out out.png
```
```bash
# Info
./cti.exe info out.cti
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
