use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde::Serialize;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymErr;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "mp4", "ogg", "oga", "wav"];
// Vorbis comment key as written by fpcalc.py/Picard; a legacy spaced variant is replaced on write.
const TAG: &str = "ACOUSTID_FINGERPRINT";
const TAG_SPACED: &str = "ACOUSTID FINGERPRINT";
// Picard's ID3/MP4 spelling; read, and replaced by TAG on write.
const TAG_PICARD: &str = "Acoustid Fingerprint";

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Txt,
    Json,
}

/// AcoustID fingerprinter for audio files.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Files or directories (scanned recursively)
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Report format written to stdout
    #[arg(short, long, value_enum, default_value_t = Format::Txt)]
    format: Format,

    /// Write the fingerprint tag into the files (files that already have one are skipped)
    #[arg(short, long)]
    write_tags: bool,

    /// Do not print a report (useful with --write-tags)
    #[arg(short, long)]
    quiet: bool,

    /// Recompute and overwrite existing fingerprints
    #[arg(long)]
    force: bool,

    /// Seconds of audio to fingerprint (AcoustID matches on the first 120)
    #[arg(short, long, default_value_t = 120)]
    length: u32,
}

#[derive(Serialize)]
struct Track {
    filename: String,
    duration_seconds: f64,
    fingerprint: String,
    /// "new", "kept" (tag already present) or "written"
    status: &'static str,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes up to `seconds` of audio and returns (duration of decoded audio, fingerprint).
fn fingerprint(path: &Path, seconds: u32) -> Result<(f64, String)> {
    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions { enable_gapless: true, ..Default::default() }, &MetadataOptions::default())?
        .format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    let mut started = false;
    let (mut rate, mut channels) = (0u32, 0usize);
    let mut frames = 0u64;
    let mut buf: Option<SampleBuffer<i16>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymErr::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymErr::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        if !started {
            rate = spec.rate;
            channels = spec.channels.count();
            printer.start(rate, channels as u32).map_err(|e| anyhow!("{e:?}"))?;
            started = true;
        }
        let b = buf.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, spec));
        if b.capacity() < decoded.capacity() * channels {
            *b = SampleBuffer::new(decoded.capacity() as u64, spec);
        }
        b.copy_interleaved_ref(decoded);
        printer.consume(b.samples());
        frames += (b.len() / channels) as u64;
        if frames >= rate as u64 * seconds as u64 {
            break;
        }
    }
    if !started {
        bail!("no audio decoded");
    }
    printer.finish();
    let compressed = FingerprintCompressor::from(&config).compress(printer.fingerprint());
    Ok((frames as f64 / rate as f64, base64url(&compressed)))
}

/// Reads the fingerprint tag; with `set`, replaces it (other tags are untouched, unchanged
/// tags are never rewritten). Tags are read and written in one step, immediately before
/// saving, so other tools (rsgain, drcalc) that ran while we were decoding keep their changes.
fn tag(path: &Path, set: Option<&str>) -> Result<Option<String>> {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::id3::v2::{Id3v2Tag, Id3v2Version};
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::ogg::tag::VorbisComments;

    let mut f = OpenOptions::new().read(true).write(set.is_some()).open(path)?;
    let po = ParseOptions::new();
    let mut wo = WriteOptions::default();
    let clean = |s: Option<&str>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let vc = |t: &mut VorbisComments| -> Option<String> {
        let old = clean(t.get(TAG).or_else(|| t.get(TAG_SPACED)));
        if let Some(fp) = set {
            t.remove(TAG_SPACED).for_each(drop);
            t.insert(TAG.to_string(), fp.to_string());
        }
        old
    };
    let id3 = |t: &mut Id3v2Tag, wo: &mut WriteOptions| -> Option<String> {
        let old = clean(t.get_user_text(TAG).or_else(|| t.get_user_text(TAG_PICARD)));
        if let Some(fp) = set {
            // Keep ID3v2.3 files at v2.3 instead of silently upgrading them to v2.4.
            if t.original_version() == Id3v2Version::V3 {
                *wo = WriteOptions::default().use_id3v23(true);
            }
            t.remove_user_text(TAG_PICARD);
            t.insert_user_text(TAG.to_string(), fp.to_string());
        }
        old
    };
    let freeform = |name: &str| AtomIdent::Freeform { mean: "com.apple.iTunes".into(), name: name.to_string().into() };
    let ilst = |t: &mut Ilst| -> Option<String> {
        let text = |t: &Ilst, n: &str| match t.get(&freeform(n)).and_then(|a| a.data().next()) {
            Some(AtomData::UTF8(s)) => clean(Some(s)),
            _ => None,
        };
        let old = text(t, TAG).or_else(|| text(t, TAG_PICARD));
        if let Some(fp) = set {
            t.remove(&freeform(TAG_PICARD)).for_each(drop);
            t.remove(&freeform(TAG)).for_each(drop);
            t.insert(Atom::new(freeform(TAG), AtomData::UTF8(fp.to_string())));
        }
        old
    };

    macro_rules! run {
        ($file:ty, $get:ident, $get_mut:ident, $set:ident, $new:expr, $apply:expr) => {{
            let mut file = <$file>::read_from(&mut f, po)?;
            if file.$get().is_none() {
                file.$set($new);
            }
            let old = $apply(file.$get_mut().unwrap());
            if set.is_some() {
                f.rewind()?;
                file.save_to(&mut f, wo)?;
            }
            old
        }};
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    Ok(match ext.as_str() {
        "flac" => run!(lofty::flac::FlacFile, vorbis_comments, vorbis_comments_mut, set_vorbis_comments, VorbisComments::default(), vc),
        "mp3" => run!(lofty::mpeg::MpegFile, id3v2, id3v2_mut, set_id3v2, Id3v2Tag::default(), |t: &mut Id3v2Tag| id3(t, &mut wo)),
        "wav" => run!(lofty::iff::wav::WavFile, id3v2, id3v2_mut, set_id3v2, Id3v2Tag::default(), |t: &mut Id3v2Tag| id3(t, &mut wo)),
        "m4a" | "mp4" => run!(lofty::mp4::Mp4File, ilst, ilst_mut, set_ilst, Ilst::default(), ilst),
        "ogg" | "oga" => {
            let mut file = lofty::ogg::VorbisFile::read_from(&mut f, po)?;
            let old = vc(file.vorbis_comments_mut());
            if set.is_some() {
                f.rewind()?;
                file.save_to(&mut f, wo)?;
            }
            old
        }
        _ => bail!("unsupported file type .{ext}"),
    })
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn collect(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in paths {
        if root.is_file() {
            out.push(root.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.map_err(|err| eprintln!("warning: {err}")).ok())
        {
            let p = entry.path();
            // "._*" are AppleDouble sidecars, not audio.
            if entry.file_type().is_file() && is_audio(p) && !p.file_name().unwrap().to_string_lossy().starts_with("._") {
                out.push(p.to_path_buf());
            }
        }
    }
    out
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let files = collect(&cli.paths);

    let results: Vec<(PathBuf, Result<Track>)> = files
        .into_par_iter()
        .map(|p| {
            let r = (|| -> Result<Track> {
                let existing = if cli.force { None } else { tag(&p, None)? };
                let (duration, fp, status) = match existing {
                    Some(fp) => (0.0, fp, "kept"),
                    None => {
                        let (d, fp) = fingerprint(&p, cli.length)?;
                        (d, fp, "new")
                    }
                };
                let mut status = status;
                if cli.write_tags && status == "new" {
                    tag(&p, Some(&fp))?;
                    status = "written";
                }
                Ok(Track { filename: p.display().to_string(), duration_seconds: (duration * 100.0).round() / 100.0, fingerprint: fp, status })
            })();
            (p, r)
        })
        .collect();

    let mut failed = false;
    let mut tracks = Vec::new();
    for (p, r) in results {
        match r {
            Ok(t) => tracks.push(t),
            Err(e) => {
                failed = true;
                eprintln!("error: {}: {e:#}", p.display());
            }
        }
    }

    if !cli.quiet {
        let mut out = std::io::stdout().lock();
        match cli.format {
            Format::Json => writeln!(out, "{}", serde_json::to_string_pretty(&tracks)?)?,
            Format::Txt => {
                for t in &tracks {
                    writeln!(out, "{}\t{}\t{}", t.status, t.filename, t.fingerprint)?;
                }
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64url_unpadded() {
        assert_eq!(super::base64url(&[1, 2, 3, 4]), "AQIDBA");
        assert_eq!(super::base64url(&[0xfb, 0xff]), "-_8");
    }
}
