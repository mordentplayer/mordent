//! FairPlay `cbcs` fragmented-MP4 parse + decrypt.
//!
//! Algorithm port of amdl `internal/fairplay-rip`: sanitized init (single
//! stsd entry, no seam/seig groups), per-fragment sample extraction
//! (tfhd/trun/senc), per-sample-region FairPlay decryption via the `temari`
//! crate, and a rewritten fragment (encryption boxes dropped, trun data
//! offsets rebased).

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

// ---- low-level box walker ----

#[derive(Debug, Clone)]
pub struct BoxHeader {
    /// Full box size including header.
    pub size: u64,
    pub kind: [u8; 4],
    /// Bytes consumed by the header itself.
    pub hdr_len: u64,
}

fn read_box_header(r: &mut impl std::io::Read) -> std::io::Result<BoxHeader> {
    let mut b8 = [0u8; 8];
    r.read_exact(&mut b8)?;
    let mut size = u32::from_be_bytes([b8[0], b8[1], b8[2], b8[3]]) as u64;
    let mut hdr = 8u64;
    if size == 1 {
        let mut b16 = [0u8; 8];
        r.read_exact(&mut b16)?;
        size = u64::from_be_bytes(b16);
        hdr = 16;
    }
    Ok(BoxHeader {
        size,
        kind: [b8[4], b8[5], b8[6], b8[7]],
        hdr_len: hdr,
    })
}

fn kind_str(k: &[u8]) -> String {
    k.iter().map(|&c| c as char).collect()
}

/// Read one full box (header + payload) from a stream.
fn read_box(r: &mut impl std::io::Read) -> std::io::Result<(BoxHeader, Vec<u8>)> {
    let h = read_box_header(r)?;
    if h.size < h.hdr_len || h.size > 1 << 30 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("absurd box size {} ({})", h.size, kind_str(&h.kind)),
        ));
    }
    let mut payload = vec![0u8; (h.size - h.hdr_len) as usize];
    r.read_exact(&mut payload)?;
    Ok((h, payload))
}

struct BoxView<'a> {
    kind: String,
    /// Full box bytes (header + payload).
    raw: &'a [u8],
}

fn boxes_in(buf: &[u8]) -> Vec<BoxView<'_>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let size = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let kind = kind_str(&buf[i + 4..i + 8]);
        let (size, hdr) = if size == 1 {
            if i + 16 > buf.len() {
                break;
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[i + 8..i + 16]);
            (u64::from_be_bytes(b) as usize, 16)
        } else {
            (size, 8)
        };
        if size < hdr || i + size > buf.len() {
            break;
        }
        out.push(BoxView {
            kind,
            raw: &buf[i..i + size],
        });
        i += size;
    }
    out
}

fn payload<'a>(b: &'a BoxView<'a>) -> &'a [u8] {
    &b.raw[8..] // no 64-bit sizes in Apple audio inits
}

fn full_payload<'a>(b: &'a BoxView<'a>) -> Result<(&'a [u8], u8)> {
    let p = payload(b);
    if p.len() < 4 {
        bail!("fullbox {} too short", b.kind);
    }
    Ok((&p[4..], p[0]))
}

// ---- decrypted init ----

#[derive(Debug, Clone)]
pub struct TrackDecryptInfo {
    pub track_id: u32,
    /// `cbcs` crypt/skip byte-block counts from tenc (Apple uses full-sample
    /// encryption, i.e. skip == 0).
    pub crypt_byte_block: u8,
    pub skip_byte_block: u8,
    pub default_iv_size: u8,
}

pub struct DecryptedInit {
    pub bytes: Vec<u8>,
    pub tracks: HashMap<u32, TrackDecryptInfo>,
}

/// Parse + sanitize the init segment (ftyp + moov): keep one stsd entry,
/// drop `seam`/`seig` sample groups, extract per-track decrypt info.
pub fn parse_init(init: &[u8]) -> Result<DecryptedInit> {
    let top = boxes_in(init);
    let mut ftyp: Option<&[u8]> = None;
    let mut moov: Option<&[u8]> = None;
    for b in &top {
        match b.kind.as_str() {
            "ftyp" => ftyp = Some(b.raw),
            "moov" => moov = Some(b.raw),
            _ => {}
        }
    }
    let ftyp = ftyp.ok_or_else(|| anyhow!("init has no ftyp"))?;
    let moov = moov.ok_or_else(|| anyhow!("init has no moov"))?;

    let mut tracks = HashMap::new();
    let new_moov = transform_moov(moov, &mut tracks)?;
    let mut bytes = Vec::with_capacity(init.len());
    bytes.extend_from_slice(ftyp);
    bytes.extend_from_slice(&new_moov);
    Ok(DecryptedInit { bytes, tracks })
}

fn transform_moov(moov: &[u8], tracks: &mut HashMap<u32, TrackDecryptInfo>) -> Result<Vec<u8>> {
    let mut trex_track_id: Option<u32> = None;
    let mut children: Vec<Vec<u8>> = Vec::new();
    for b in boxes_in(moov_payload(moov)) {
        match b.kind.as_str() {
            "trak" => children.push(transform_trak(b.raw, tracks)?),
            "mvex" => {
                // capture trex track id so tenc info is keyed correctly
                for c in walk_children(payload(&b)) {
                    if c.kind == "trex" {
                        if let Some(p) = payload(&c).get(4..8) {
                            trex_track_id =
                                Some(u32::from_be_bytes(p.try_into().unwrap()));
                        }
                    }
                }
                children.push(b.raw.to_vec());
            }
            _ => children.push(b.raw.to_vec()),
        }
    }
    if let Some(t) = trex_track_id {
        if let Some(info) = tracks.remove(&1) {
            tracks.insert(t, TrackDecryptInfo { track_id: t, ..info });
        }
    }
    Ok(build_box("moov", children.iter().map(|v| v.as_slice())))
}

fn moov_payload(moov: &[u8]) -> &[u8] {
    &moov[8..]
}

fn walk_children(buf: &[u8]) -> Vec<BoxView<'_>> {
    boxes_in(buf)
}

fn transform_trak(trak: &[u8], tracks: &mut HashMap<u32, TrackDecryptInfo>) -> Result<Vec<u8>> {
    let mut children: Vec<Vec<u8>> = Vec::new();
    for b in walk_children(&trak[8..]) {
        if b.kind == "mdia" {
            children.push(transform_mdia(b.raw, tracks)?);
        } else {
            children.push(b.raw.to_vec());
        }
    }
    Ok(build_box("trak", children.iter().map(|v| v.as_slice())))
}

fn transform_mdia(mdia: &[u8], tracks: &mut HashMap<u32, TrackDecryptInfo>) -> Result<Vec<u8>> {
    let mut children: Vec<Vec<u8>> = Vec::new();
    for b in walk_children(&mdia[8..]) {
        if b.kind == "minf" {
            children.push(transform_minf(b.raw, tracks)?);
        } else {
            children.push(b.raw.to_vec());
        }
    }
    Ok(build_box("mdia", children.iter().map(|v| v.as_slice())))
}

fn transform_minf(minf: &[u8], tracks: &mut HashMap<u32, TrackDecryptInfo>) -> Result<Vec<u8>> {
    let mut children: Vec<Vec<u8>> = Vec::new();
    for b in walk_children(&minf[8..]) {
        if b.kind == "stbl" {
            children.push(transform_stbl(b.raw, tracks)?);
        } else {
            children.push(b.raw.to_vec());
        }
    }
    Ok(build_box("minf", children.iter().map(|v| v.as_slice())))
}

fn transform_stbl(stbl: &[u8], tracks: &mut HashMap<u32, TrackDecryptInfo>) -> Result<Vec<u8>> {
    let mut children: Vec<Vec<u8>> = Vec::new();
    let mut track_id_hint = 0u32;
    for b in walk_children(&stbl[8..]) {
        match b.kind.as_str() {
            "sgpd" | "sbgp" => {
                // drop seam/seig encryption grouping; keep others
                let g = payload(&b).get(4..8);
                let keep = g != Some(&b"seam"[..]) && g != Some(&b"seig"[..]);
                if keep {
                    children.push(b.raw.to_vec());
                }
            }
            "stsd" => children.push(sanitize_stsd(b.raw, &mut track_id_hint, tracks)?),
            _ => children.push(b.raw.to_vec()),
        }
    }
    Ok(build_box("stbl", children.iter().map(|v| v.as_slice())))
}

/// stsd: version/flags + entry_count + sample entries. Keep only the first
/// entry (the clear one) and pull tenc info from the encrypted entry's sinf.
fn sanitize_stsd(
    stsd: &[u8],
    track_id: &mut u32,
    tracks: &mut HashMap<u32, TrackDecryptInfo>,
) -> Result<Vec<u8>> {
    let p = &stsd[8..];
    if p.len() < 8 {
        bail!("stsd too short");
    }
    let entry_count = u32::from_be_bytes([p[4], p[5], p[6], p[7]]) as usize;
    let entries = boxes_in(&p[8..]);
    if entries.is_empty() {
        bail!("stsd has no entries");
    }
    // walk every entry; find sinf/schi/tenc in the encrypted one(s), and
    // build the CLEAR entry: same bytes, kind = frma 4cc, sinf dropped
    let mut clear_entry: Option<Vec<u8>> = None;
    for e in &entries {
        let mut info = None;
        let mut frma = None;
        for child in walk_children(payload_of_entry(e)) {
            if child.kind == "sinf" {
                info = extract_tenc(&child);
                for c2 in walk_children(payload(&child)) {
                    if c2.kind == "frma" {
                        frma = payload(&c2).get(0..4).map(|b| b.to_vec());
                    }
                }
            }
        }
        if let Some(info) = info {
            *track_id = 1; // single-track audio; re-keyed via trex in transform_moov
            tracks.entry(*track_id).or_insert(info);
            let kind = frma.unwrap_or_else(|| b"alac".to_vec());
            let mut body: Vec<u8> = Vec::with_capacity(e.raw.len());
            body.extend_from_slice(&kind);
            body.extend_from_slice(&e.raw[8..8 + 28]); // fixed AudioSampleEntry fields
            for child in walk_children(payload_of_entry(e)) {
                if child.kind != "sinf" {
                    body.extend_from_slice(child.raw);
                }
            }
            let mut out = Vec::with_capacity(body.len() + 8);
            out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
            out.extend_from_slice(&body);
            clear_entry = Some(out);
        } else if clear_entry.is_none() {
            clear_entry = Some(e.raw.to_vec());
        }
    }
    if entry_count > 2 {
        bail!("expected 1 or 2 stsd entries, got {entry_count}");
    }
    let entry_bytes =
        clear_entry.ok_or_else(|| anyhow!("stsd has no usable sample entry"))?;
    // rebuild stsd with only the clear entry
    let mut body = Vec::new();
    body.extend_from_slice(&p[..4]); // version/flags
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&entry_bytes);
    Ok(build_box("stsd", [body.as_slice()].into_iter()))
}

/// Sample entry payload: skip AudioSampleEntry fixed fields (8 hdr + 6+2+8+2+2+2+2+4 = 36)
/// then child boxes.
fn payload_of_entry<'a>(e: &'a BoxView<'a>) -> &'a [u8] {
    &e.raw[8 + 28..]
}

fn extract_tenc(sinf: &BoxView) -> Option<TrackDecryptInfo> {
    let schm = walk_children(payload(sinf)).into_iter().find(|b| b.kind == "schi")?;
    let tenc = walk_children(payload(&schm)).into_iter().find(|b| b.kind == "tenc")?;
    let (p, version) = full_payload(&tenc).ok()?;
    let mut i = 0usize;
    let _reserved = p.get(i)?; // reserved byte(s)
    let (crypt, skip) = if version == 0 {
        (0u8, 0u8)
    } else {
        let v = *p.get(i + 1)?;
        (v >> 4, v & 0xf)
    };
    i += 2;
    let is_protected = *p.get(i)?;
    i += 1;
    let iv_size = *p.get(i)?;
    i += 1;
    let _kid = p.get(i..i + 16)?;
    if is_protected == 0 {
        return None;
    }
    Some(TrackDecryptInfo {
        track_id: 1,
        crypt_byte_block: crypt,
        skip_byte_block: skip,
        default_iv_size: iv_size,
    })
}

fn build_box<'a>(kind: &str, children: impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for c in children {
        body.extend_from_slice(c);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind.as_bytes());
    out.extend_from_slice(&body);
    out
}

// ---- fragment decryption ----

pub struct Fragment {
    /// Rewritten fragment bytes (encryption removed).
    pub bytes: Vec<u8>,
}

/// Decrypt one fragment (moof…mdat) with the given template.
///
/// `info` must come from [`parse_init`]. `tmpl` is the key template for this
/// fragment's skd URI. Returns the rewritten (clear) fragment.
pub fn decrypt_fragment(
    moof_raw: &[u8],
    mdat_raw: &[u8],
    info: &TrackDecryptInfo,
    tmpl: &temari::rounds::Template,
) -> Result<Vec<u8>> {
    if info.skip_byte_block != 0 {
        bail!("partial (skip-block) encryption unsupported");
    }
    // moof → traf(s) → senc + trun(s); moof_raw starts with the moof header,
    // so children begin at offset 8
    let moof_children = boxes_in(&moof_raw[8..]);
    let mut traf_view = None;
    for b in &moof_children {
        if b.kind == "traf" {
            traf_view = Some(b);
        }
    }
    let traf = traf_view.ok_or_else(|| anyhow!("moof without traf"))?;

    // tfhd: track id + flags
    let mut track_id = None;
    let mut base_data_offset: Option<u64> = None;
    let mut default_size: Option<u32> = None;
    for b in walk_children(payload(traf)) {
        if b.kind == "tfhd" {
            let (p, _v) = full_payload(&b)?;
            let flags = u32::from_be_bytes([b.raw[8], b.raw[9], b.raw[10], b.raw[11]]);
            let mut i = 0usize;
            track_id = Some(u32::from_be_bytes(p[i..i + 4].try_into()?));
            i += 4;
            if flags & 0x1 != 0 {
                base_data_offset = Some(u64::from_be_bytes(p[i..i + 8].try_into()?));
                i += 8;
            }
            if flags & 0x2 != 0 {
                i += 4; // sample description index
            }
            if flags & 0x8 != 0 {
                i += 4; // default sample duration
            }
            if flags & 0x10 != 0 {
                default_size = Some(u32::from_be_bytes(p[i..i + 4].try_into()?));
                i += 4;
            }
            if flags & 0x20 != 0 {
                i += 4; // default sample flags
            }
        }
    }
    let track_id = track_id.unwrap_or(1);
    let _ = base_data_offset;
    let _ = default_size;
    if track_id != info.track_id && info.track_id != 0 {
        // single-track audio: tolerate id mismatch between stsd hint and tfhd
    }

    // senc: sample count + optional per-sample IV/subsample patterns
    let senc = walk_children(payload(traf)).into_iter().find(|b| b.kind == "senc");
    let mut subsamples: Vec<Vec<(u16, u32)>> = Vec::new();
    let mut iv_total = 0usize;
    if let Some(s) = &senc {
        let (p, flags) = full_payload(s)?;
        let count = u32::from_be_bytes(p[0..4].try_into()?) as usize;
        let per_sample_sub = flags & 0x2 != 0;
        let mut i = 4usize;
        for _ in 0..count {
            if info.default_iv_size > 0 {
                i += info.default_iv_size as usize;
            }
            let mut pats = Vec::new();
            if per_sample_sub {
                let n = u16::from_be_bytes(p[i..i + 2].try_into()?) as usize;
                i += 2;
                for _ in 0..n {
                    let clear = u16::from_be_bytes(p[i..i + 2].try_into()?);
                    let prot = u32::from_be_bytes(p[i + 2..i + 6].try_into()?);
                    pats.push((clear, prot));
                    i += 6;
                }
            }
            subsamples.push(pats);
        }
        iv_total = i - 4 - (if per_sample_sub { 0 } else { 0 });
        let _ = iv_total;
    }

    // trun: sample sizes
    let mut sizes: Vec<u32> = Vec::new();
    let mut data_offset: i64 = 0;
    for b in walk_children(payload(traf)) {
        if b.kind != "trun" {
            continue;
        }
        let (p, _v) = full_payload(&b)?;
        let flags = u32::from_be_bytes([b.raw[8], b.raw[9], b.raw[10], b.raw[11]]);
        let count = u32::from_be_bytes(p[0..4].try_into()?) as usize;
        let mut i = 4usize;
        if flags & 0x1 != 0 {
            data_offset = i32::from_be_bytes(p[i..i + 4].try_into()?) as i64;
            i += 4;
        }
        if flags & 0x4 != 0 {
            i += 4; // first sample flags
        }
        for _ in 0..count {
            if flags & 0x100 != 0 {
                i += 4;
            }
            if flags & 0x200 != 0 {
                sizes.push(u32::from_be_bytes(p[i..i + 4].try_into()?));
                i += 4;
            } else {
                sizes.push(default_size.unwrap_or(0));
            }
            if flags & 0x400 != 0 {
                i += 4;
            }
            if flags & 0x800 != 0 {
                i += 4;
            }
        }
    }
    if sizes.iter().any(|&s| s == 0) {
        bail!("trun missing per-sample sizes and no default size");
    }

    // mdat payload start relative to moof start = moof_size + 8
    let mdat_payload_start = moof_raw.len() as i64 + 8;
    let first = data_offset - mdat_payload_start;
    if first < 0 || first as usize + sizes.iter().sum::<u32>() as usize > mdat_raw.len() {
        bail!(
            "samples out of mdat range (offset {first}, total {}, mdat {})",
            sizes.iter().sum::<u32>(),
            mdat_raw.len()
        );
    }
    let mdat_payload = &mdat_raw[8..];
    let mut pos = first as usize;

    // decrypt samples into a new mdat payload
    let mut clear_mdat: Vec<u8> = Vec::with_capacity(mdat_raw.len());
    // copy any leading clear bytes before first sample
    clear_mdat.extend_from_slice(&mdat_payload[..pos]);

    for (idx, &size) in sizes.iter().enumerate() {
        let sample = &mdat_payload[pos..pos + size as usize];
        let regions: Vec<(usize, usize)> = if subsamples.len() > idx && !subsamples[idx].is_empty() {
            let mut v = Vec::new();
            let mut p = 0usize;
            for (clear, prot) in &subsamples[idx] {
                p += *clear as usize;
                v.push((p, *prot as usize));
                p += *prot as usize;
            }
            v
        } else {
            vec![(0, size as usize)]
        };
        let mut out = Vec::with_capacity(size as usize);
        let mut last_end = 0usize;
        for (start, prot) in regions {
            out.extend_from_slice(&sample[last_end..start]);
            let region = &sample[start..start + prot];
            let trunc = region.len() & !0xf;
            if trunc > 0 {
                out.extend_from_slice(&temari::rounds::decrypt(tmpl, &region[..trunc]));
            }
            out.extend_from_slice(&region[trunc..]);
            last_end = start + prot;
        }
        out.extend_from_slice(&sample[last_end..]);
        if out.len() != size as usize {
            bail!("decrypted sample length drift: {} -> {}", size, out.len());
        }
        clear_mdat.extend_from_slice(&out);
        pos += size as usize;
    }

    // rebuild moof without senc/saiz/saio/pssh; rebase trun data offsets
    let removed_before: usize = walk_children(payload(traf))
        .iter()
        .filter(|b| matches!(b.kind.as_str(), "senc" | "saiz" | "saio" | "uuid" | "sgpd" | "sbgp"))
        .map(|b| b.raw.len())
        .sum();
    // pssh lives under moof directly, not traf; handle both
    let moof_children_removed: usize = moof_children
        .iter()
        .filter(|b| b.kind == "pssh")
        .map(|b| b.raw.len())
        .sum();

    let shift = (removed_before + moof_children_removed) as i64;
    let new_data_offset = data_offset - shift;
    let new_traf = rebuild_traf(traf, new_data_offset)?;
    let new_moof_body = {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        for b in &moof_children {
            match b.kind.as_str() {
                "mfhd" => parts.push(b.raw.to_vec()),
                "traf" => parts.push(new_traf.clone()),
                _ => {}
            }
        }
        build_box("moof", parts.iter().map(|v| v.as_slice()))
    };

    let mut out = Vec::with_capacity(new_moof_body.len() + clear_mdat.len() + 8);
    out.extend_from_slice(&new_moof_body);
    out.extend_from_slice(&((clear_mdat.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&clear_mdat);
    Ok(out)
}

fn rebuild_traf(traf: &BoxView, new_data_offset: i64) -> Result<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for b in walk_children(payload(traf)) {
        match b.kind.as_str() {
            "senc" | "saiz" | "saio" | "sgpd" | "sbgp" => {}
            "trun" => {
                let mut trun = b.raw.to_vec();
                let flags = u32::from_be_bytes([b.raw[8], b.raw[9], b.raw[10], b.raw[11]]);
                if flags & 0x1 != 0 {
                    let off_at = 8 + 8; // hdr + version/flags, then data_offset
                    trun[off_at..off_at + 4]
                        .copy_from_slice(&(new_data_offset as i32).to_be_bytes());
                }
                parts.push(trun);
            }
            _ => parts.push(b.raw.to_vec()),
        }
    }
    Ok(build_box("traf", parts.iter().map(|v| v.as_slice())))
}
