# fpscan

AcoustID fingerprinter in Rust. Scans directories recursively and writes the fingerprint into each file's tags, in the style of [rsgain](https://github.com/complexlogic/rsgain) and [drcalc](https://github.com/koehntopp/drcalc). Decoding ([symphonia](https://github.com/pdeljanov/Symphonia)), fingerprinting ([rusty-chromaprint](https://crates.io/crates/rusty-chromaprint)) and tagging ([lofty](https://crates.io/crates/lofty)) are all in-process: no `fpcalc` binary, no Python.

## Install

```sh
cargo install --git https://github.com/koehntopp/fpscan
```

## Usage

```sh
fpscan ~/Music/Album                 # report: status, file, fingerprint (tab-separated)
fpscan -f json ~/Music/Album         # JSON report
fpscan -w -q ~/Music                 # write tags only, no report
fpscan -w --force ~/Music/Album      # recompute and overwrite existing fingerprints
```

| Option | Meaning |
|---|---|
| `-f, --format txt\|json` | Report format on stdout (default `txt`) |
| `-w, --write-tags` | Write `ACOUSTID_FINGERPRINT` into the files |
| `-q, --quiet` | No report |
| `--force` | Recompute and overwrite existing fingerprints |
| `-l, --length <SECONDS>` | Audio to fingerprint (default 120; AcoustID matches on the first 120 s) |

Supported files: FLAC, MP3, M4A/MP4 (AAC, ALAC), Ogg Vorbis, WAV. Opus, Ogg FLAC and AIFF are not supported.

The exit code is 1 if any file could not be fingerprinted or tagged. Files are processed in parallel.

## Tags

| Format | Tag |
|---|---|
| FLAC, Ogg | Vorbis comment `ACOUSTID_FINGERPRINT` |
| MP3, WAV | ID3v2 `TXXX:ACOUSTID_FINGERPRINT` |
| M4A | `----:com.apple.iTunes:ACOUSTID_FINGERPRINT` |

Files that already have a fingerprint are skipped unless `--force` is given. Other tags, ReplayGain values and pictures are kept, ID3v2.3 tags stay at v2.3, and a tag is read and written in one step right before saving, so tools running in parallel (rsgain, drcalc) keep their changes. The legacy `ACOUSTID FINGERPRINT` and Picard-style `Acoustid Fingerprint` keys are read, and replaced by `ACOUSTID_FINGERPRINT` when a file is rewritten.

## Compatibility with fpcalc

Fingerprints use the same algorithm and encoding as `fpcalc` (Chromaprint algorithm 2, compressed base64), but are not byte-identical: decoding and resampling differ from FFmpeg's. On a sample of FLAC and M4A files the raw fingerprints differed by 0.3-1.4% of bits, far inside what AcoustID matching tolerates. Fingerprints already written by `fpcalc` are left alone unless you pass `--force`.

## License

MIT
