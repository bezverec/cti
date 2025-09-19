# cti
Custom Tiled Images
## Build from source

$env:RUSTFLAGS="-C target-cpu=native"; cargo build --release

## Quickstart
```bash
# NDK preset
./cti.exe encode in.tif out.cti --ndk
```
```bash
# Validation and PNG Preview (8bpc)
./ct.exei decode out.cti out.raw --png-out out.png
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
