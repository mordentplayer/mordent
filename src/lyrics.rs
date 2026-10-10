//! Time-synced lyrics for the karaoke panel: fetch + parse + pure timeline.
//!
//! ## Data shape (discovered live against wrapper-lite, 2026-10)
//!
//! `GET /lyrics?adamId=<songId>&syllable=0|1` answers the usual wrapper
//! envelope (`{"code":0,"msg":"SUCCESS","data":…}`; a track without lyrics is
//! `{"code":404,"msg":"lyrics not found"}` — see `wrapper::Wrapper::lyrics_ttml`):
//!
//! ```json
//! {"code":0,"msg":"SUCCESS","data":{
//!   "adamId":"697195462","syllable":"false","lyrics":"<tt …>…</tt>"}}
//! ```
//!
//! `lyrics` is **TTML XML, not JSON** (the only place in this client that is
//! true). Both modes were captured and inspected:
//!
//! * `syllable=0` → `itunes:timing="Line"`, `<p begin="30.237" end="31.926">One more time</p>`
//!   (plain text, no children);
//! * `syllable=1` → `itunes:timing="Word"`, the same `<p>` with one timed
//!   `<span begin end>` per word: `<span begin="30.237" end="30.749">One</span>`
//!   — word timing is a strict superset of line timing (the `<p>` bounds are
//!   still there), which is why the UI asks for it and falls back to line mode.
//!
//! Timestamps are `ss.mmm` **or** `mm:ss.mmm` (fractional seconds with ms
//! precision; `body dur="5:20.357"` uses the same grammar). They are absolute
//! track positions, not offsets.
//!
//! Background vocals are nested untimed spans: `<span ttm:role="x-bg"><span
//! begin end>word</span>…</span>`. Those inner words carry real timing and
//! overlap the lead line, so they are kept but flagged ([`Word::bg`]) and never
//! win a position lookup over a lead word (the UI dims them).
//!
//! Text may carry XML entities (`&amp;`, `&#39;`, …) — decoded here.
//!
//! No XML crate is a dependency (and we add none), so the two tags this view
//! needs (`<p>`, `<span>`) are hand-parsed. `parser::Attributes` in TTML is
//! irrelevant to us: only `begin`, `end` and `ttm:role` are read.

use crate::config::Config;
use crate::wrapper::Wrapper;
use anyhow::Result;

/// One timed word (or syllable) inside a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub start_ms: u64,
    /// Exclusive: the word is active on `start_ms..end_ms`.
    pub end_ms: u64,
    pub text: String,
    /// Background vocal (`ttm:role="x-bg"`) — rendered dimmed, never preferred
    /// by [`LyricTimeline::word_at`].
    pub bg: bool,
}

/// One lyric line: its own span, its plain text, and (word-timed documents
/// only) its words in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    pub words: Vec<Word>,
}

/// Parsed lyrics for one track, ordered by `start_ms`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LyricTimeline {
    pub lines: Vec<LyricLine>,
}

/// Outcome of a lyrics fetch that reached the wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// Parsed document. Never empty: an empty document is reported as
    /// [`Fetched::NoLyrics`].
    Timeline(LyricTimeline),
    /// The wrapper answered 404: this track has no lyrics.
    NoLyrics,
}

/// Fetch (and parse) the lyrics for `song_id`. `Err` means the wrapper could
/// not be reached / errored — the UI's "lyrics unavailable offline" state.
pub fn fetch(cfg: &Config, song_id: &str, syllable: bool) -> Result<Fetched> {
    let w = Wrapper::new(&cfg.wrapper_url);
    // Word timing carries line timing too, so it is the better request; only
    // if it yields nothing (absent or unparsable) try the line-only document.
    if syllable {
        if let Some(ttml) = w.lyrics_ttml(song_id, true)? {
            let tl = LyricTimeline::parse(&ttml);
            if !tl.is_empty() {
                return Ok(Fetched::Timeline(tl));
            }
        }
        return match w.lyrics_ttml(song_id, false)? {
            Some(ttml) => Ok(non_empty(LyricTimeline::parse(&ttml))),
            None => Ok(Fetched::NoLyrics),
        };
    }
    match w.lyrics_ttml(song_id, false)? {
        Some(ttml) => Ok(non_empty(LyricTimeline::parse(&ttml))),
        None => Ok(Fetched::NoLyrics),
    }
}

fn non_empty(tl: LyricTimeline) -> Fetched {
    if tl.is_empty() {
        Fetched::NoLyrics
    } else {
        Fetched::Timeline(tl)
    }
}

/// One `<p>` as parsed, before the timing fallbacks are applied.
struct RawLine {
    begin: Option<u64>,
    end: Option<u64>,
    text: String,
    words: Vec<Word>,
}

impl LyricTimeline {
    /// Parse a TTML document. Never fails: a document with no usable `<p>`
    /// yields an empty timeline (the UI shows its "no lyrics" state).
    pub fn parse(ttml: &str) -> Self {
        let mut raw: Vec<RawLine> = Vec::new();
        let mut rest = ttml;
        while let Some(lt) = rest.find("<p") {
            let after = &rest[lt + 2..];
            let Some(gt) = after.find('>') else { break };
            let head = &after[..gt];
            // `<path …>` / `<pfoo …>`: not our tag — resume after this "<p".
            if head.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                rest = &rest[lt + 2..];
                continue;
            }
            let body_at = lt + 2 + gt + 1;
            let (inner, next) = match rest[body_at..].find("</p>") {
                Some(k) => (&rest[body_at..body_at + k], body_at + k + 4),
                None => (&rest[body_at..], rest.len()),
            };
            raw.push(RawLine {
                begin: attr(head, "begin").and_then(parse_ts),
                end: attr(head, "end").and_then(parse_ts),
                text: plain_text(inner),
                words: parse_words(inner),
            });
            rest = &rest[next..];
        }

        let starts: Vec<u64> = raw
            .iter()
            .scan(0u64, |prev_end, line| {
                // A line without `begin` (never seen live) starts where the
                // previous one ended — an ordered document still reads right.
                let s = line.begin.unwrap_or(*prev_end);
                *prev_end = s;
                Some(s)
            })
            .collect();

        let mut lines: Vec<LyricLine> = raw
            .into_iter()
            .enumerate()
            .map(|(i, RawLine { end, text, words, .. })| {
                let start = starts[i];
                let next = starts.get(i + 1).copied();
                // A missing/degenerate `end` runs to the next line's start;
                // with no next line it stays active for at least one ms.
                let end = end
                    .filter(|e| *e > start)
                    .or(next.filter(|e| *e > start))
                    .unwrap_or(start + 1);
                LyricLine {
                    start_ms: start,
                    end_ms: end,
                    text,
                    words,
                }
            })
            .collect();
        // `line_at` binary-searches, so guarantee ordering even for a
        // malformed document (live documents are already in order).
        lines.sort_by_key(|l| l.start_ms);
        Self { lines }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Index of the line active at `ms` — `start_ms <= ms < end_ms`, so the
    /// start timestamp is inclusive and the end exclusive. `None` before the
    /// first line, inside a gap between lines, and after the last one.
    pub fn line_at(&self, ms: u64) -> Option<usize> {
        let i = self.lines.partition_point(|l| l.start_ms <= ms);
        let idx = i.checked_sub(1)?;
        (ms < self.lines[idx].end_ms).then_some(idx)
    }

    /// Index of the word active at `ms` *within* line `line`, same interval
    /// rule as [`Self::line_at`]. A lead word always beats a background word
    /// overlapping it; `None` when the line has no word timing or nothing
    /// matches.
    pub fn word_at(&self, ms: u64, line: usize) -> Option<usize> {
        let words = &self.lines.get(line)?.words;
        let mut bg_hit = None;
        for (i, w) in words.iter().enumerate() {
            if w.start_ms <= ms && ms < w.end_ms {
                if !w.bg {
                    return Some(i);
                }
                bg_hit.get_or_insert(i);
            }
        }
        bg_hit
    }
}

/// `ss.mmm`, `mm:ss.mmm` or `hh:mm:ss.mmm` → milliseconds.
fn parse_ts(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut parts = s.split(':').peekable();
    while let Some(p) = parts.next() {
        if parts.peek().is_some() {
            total = total.checked_mul(60)?.checked_add(p.trim().parse().ok()?)?;
            continue;
        }
        let (sec, frac) = p.split_once('.').unwrap_or((p, ""));
        total = total.checked_mul(60)?.checked_add(sec.trim().parse().ok()?)?;
        if frac.is_empty() {
            return total.checked_mul(1000);
        }
        let digits: String = frac.chars().take(3).collect();
        let mut ms: u64 = digits.parse().ok()?;
        for _ in digits.len()..3 {
            ms *= 10;
        }
        return total.checked_mul(1000)?.checked_add(ms);
    }
    None
}

/// Value of `name` in a tag head like ` begin="1:03.354" itunes:key="L3"`.
/// Only matches at a tag-boundary position, so `begin` never matches inside
/// `xbegin=` or inside another attribute's value.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for (i, _) in tag.char_indices() {
        if !tag[i..].starts_with(name) {
            continue;
        }
        let Some(rest) = tag[i + name.len()..].strip_prefix('=') else {
            continue;
        };
        if i != 0 && !tag[..i].ends_with(char::is_whitespace) {
            continue;
        }
        let Some(q) = rest.chars().next() else { return Some("") };
        if q == '"' || q == '\'' {
            let end = rest[1..].find(q)?;
            return Some(&rest[1..1 + end]);
        }
        return Some(rest.trim_end());
    }
    None
}

/// Timed `<span>`s inside a `<p>`; untimed spans are `x-bg` wrappers (their
/// nested children remain true words) or ignored text containers.
fn parse_words(inner: &str) -> Vec<Word> {
    struct Frame {
        /// `None` for an untimed (wrapper) span.
        span: Option<(u64, u64)>,
        text: String,
        bg: bool,
        /// This frame is itself an `x-bg` wrapper: pop decrements bg depth.
        wrapper: bool,
    }
    let mut words = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut bg_depth = 0usize;
    let mut i = 0;
    while let Some(lt) = inner[i..].find('<') {
        let lt = i + lt;
        if let Some(top) = stack.last_mut() {
            top.text.push_str(&inner[i..lt]);
        }
        let Some(gt) = inner[lt..].find('>') else { break };
        let gt = lt + gt;
        let tag = &inner[lt + 1..gt];
        if let Some(name) = tag.strip_prefix('/') {
            if name == "span"
                && let Some(f) = stack.pop()
            {
                if f.wrapper {
                    bg_depth = bg_depth.saturating_sub(1);
                }
                if let Some((begin, end)) = f.span {
                    words.push(Word {
                        start_ms: begin,
                        end_ms: end,
                        text: decode(&f.text).trim().to_string(),
                        bg: f.bg,
                    });
                }
            }
        } else if tag == "span" || tag.starts_with("span ") {
            let begin = attr(tag, "begin").and_then(parse_ts);
            let end = attr(tag, "end").and_then(parse_ts);
            let role = attr(tag, "ttm:role").or_else(|| attr(tag, "role"));
            let wrapper = role == Some("x-bg");
            if wrapper {
                bg_depth += 1;
            }
            stack.push(Frame {
                span: match (begin, end) {
                    (Some(b), Some(e)) if e > b => Some((b, e)),
                    (Some(b), None) => Some((b, b + 1)),
                    _ => None,
                },
                text: String::new(),
                bg: bg_depth > 0,
                wrapper,
            });
        }
        i = gt + 1;
    }
    words
}

/// XML entities → chars (`&amp;`, `&apos;`, `&#39;`, `&#x27;`, …). Unknown
/// entities are kept as-is.
fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let ent_len = match tail.find(';') {
            Some(k) if k <= 12 => k,
            _ => {
                out.push('&');
                rest = &tail[1..];
                continue;
            }
        };
        let ent = &tail[1..ent_len];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[ent_len + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tag-stripped, entity-decoded, whitespace-collapsed line text — the display
/// string in line mode, and the fallback when a word-timed line is not the
/// active one.
fn plain_text(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic fixtures: invented lyrics, never real ones. Both timings were
    /// trimmed by hand from live documents (see the module docs) — the
    /// structure is what matters, the words are ours.
    const LINE_TTML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" itunes:timing="Line" xml:lang="en">
<head><metadata><iTunesMetadata leadingSilence="0.020"><translations/></iTunesMetadata></metadata></head>
<body dur="0:12.000">
<div begin="1.000" end="4.000" itunes:songPart="Verse">
<p begin="1.000" end="2.000" itunes:key="L1" ttm:agent="v1">Rust moves fast</p>
<p begin="2.500" end="4.000" itunes:key="L2" ttm:agent="v1">Zeros &amp; ones &#39;til dawn</p>
</div>
</body></tt>"#;

    const WORD_TTML: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" itunes:timing="Word" xml:lang="en">
<body dur="0:12.000">
<div begin="1.000" end="4.000" itunes:songPart="Verse">
<p begin="1.000" end="2.000" itunes:key="L1" ttm:agent="v1"><span begin="1.000" end="1.400">Rust</span> <span begin="1.400" end="1.700">moves</span> <span begin="1.700" end="2.000">fast</span></p>
<p begin="2.500" end="4.000" itunes:key="L2" ttm:agent="v1"><span begin="2.500" end="2.900">Zero-cost</span> <span begin="2.900" end="3.300">&amp; bold</span> <span ttm:role="x-bg"><span begin="3.000" end="3.600">(bold)</span></span></p>
</div>
</body></tt>"#;

    fn words(tl: &LyricTimeline, line: usize) -> Vec<&Word> {
        tl.lines[line].words.iter().collect()
    }

    #[test]
    fn parses_line_timing() {
        let tl = LyricTimeline::parse(LINE_TTML);
        assert_eq!(tl.lines.len(), 2);
        let l0 = &tl.lines[0];
        assert_eq!((l0.start_ms, l0.end_ms), (1000, 2000));
        assert_eq!(l0.text, "Rust moves fast");
        assert!(l0.words.is_empty(), "line documents carry no spans");
        // entities decoded, whitespace collapsed
        assert_eq!(tl.lines[1].text, "Zeros & ones 'til dawn");
    }

    #[test]
    fn parses_word_timing_and_marks_background_vocals() {
        let tl = LyricTimeline::parse(WORD_TTML);
        assert_eq!(tl.lines.len(), 2);
        // the word document still carries the line bounds
        assert_eq!((tl.lines[1].start_ms, tl.lines[1].end_ms), (2500, 4000));
        assert_eq!(tl.lines[1].text, "Zero-cost & bold (bold)");
        let w = words(&tl, 1);
        assert_eq!(w.len(), 3);
        assert_eq!(
            (w[0].start_ms, w[0].end_ms, w[0].text.as_str(), w[0].bg),
            (2500, 2900, "Zero-cost", false)
        );
        assert_eq!(w[1].text, "& bold");
        assert_eq!((w[2].start_ms, w[2].end_ms, w[2].text.as_str(), w[2].bg), (3000, 3600, "(bold)", true));
    }

    #[test]
    fn line_at_boundaries() {
        let tl = LyricTimeline::parse(WORD_TTML);
        assert_eq!(tl.line_at(0), None); // before the first line
        assert_eq!(tl.line_at(999), None);
        assert_eq!(tl.line_at(1000), Some(0)); // start is inclusive
        assert_eq!(tl.line_at(1999), Some(0));
        assert_eq!(tl.line_at(2000), None); // end is exclusive → gap
        assert_eq!(tl.line_at(2499), None); // inside the gap
        assert_eq!(tl.line_at(2500), Some(1));
        assert_eq!(tl.line_at(3999), Some(1));
        assert_eq!(tl.line_at(4000), None); // after the last line
        assert_eq!(tl.line_at(u64::MAX), None);
    }

    #[test]
    fn word_at_boundaries() {
        let tl = LyricTimeline::parse(WORD_TTML);
        assert_eq!(tl.word_at(999, 0), None); // before the first word
        assert_eq!(tl.word_at(1000, 0), Some(0));
        assert_eq!(tl.word_at(1399, 0), Some(0));
        assert_eq!(tl.word_at(1400, 0), Some(1)); // exact next start
        assert_eq!(tl.word_at(2000, 0), None); // exclusive end, after last word
        assert_eq!(tl.word_at(9999, 0), None);
        assert_eq!(tl.word_at(1500, 9), None); // no such line
        assert_eq!(tl.word_at(1500, 1), None); // line without a word timing match
    }

    #[test]
    fn word_at_prefers_lead_over_overlapping_background() {
        let tl = LyricTimeline::parse(WORD_TTML);
        // 3000 ms: lead word "& bold" (index 1) overlaps bg "(bold)" (index 2)
        assert_eq!(tl.word_at(3000, 1), Some(1));
        assert_eq!(tl.word_at(3299, 1), Some(1));
        // 3400 ms: only the background word is active
        assert_eq!(tl.word_at(3400, 1), Some(2));
    }

    #[test]
    fn parse_timestamp_grammars() {
        assert_eq!(parse_ts("30.237"), Some(30_237));
        assert_eq!(parse_ts("1:03.354"), Some(63_354));
        assert_eq!(parse_ts("5:20.357"), Some(320_357));
        assert_eq!(parse_ts("1:02:03.500"), Some(3_723_500));
        assert_eq!(parse_ts("7"), Some(7_000));
        assert_eq!(parse_ts("0:00.000"), Some(0));
        assert_eq!(parse_ts(""), None);
        assert_eq!(parse_ts("later"), None);
    }

    #[test]
    fn attribute_lookup_is_boundary_anchored() {
        assert_eq!(attr(r#" begin="1:03.354" itunes:key="L3""#, "begin"), Some("1:03.354"));
        assert_eq!(attr(r#" itunes:key="L3" begin="2.000""#, "begin"), Some("2.000"));
        // never matches inside a longer name or a value
        assert_eq!(attr(r#" xbegin="9.000""#, "begin"), None);
        assert_eq!(attr(r#" itunes:key="begin""#, "end"), None);
        assert_eq!(attr(r#" end="4.000""#, "end"), Some("4.000"));
    }

    #[test]
    fn missing_end_runs_to_the_next_line_then_one_ms() {
        let tl = LyricTimeline::parse(
            r#"<tt><body><p begin="5.000">late</p><p begin="9.000">last</p></body></tt>"#,
        );
        assert_eq!(tl.lines[0].end_ms, 9000, "no end → the next line's start");
        assert_eq!(tl.lines[1].end_ms, 9001, "no next line → one ms");
        assert_eq!(tl.line_at(8999), Some(0));
        assert_eq!(tl.line_at(9000), Some(1));
        assert_eq!(tl.line_at(9001), None);
    }

    #[test]
    fn unparsable_or_empty_documents_yield_an_empty_timeline() {
        for doc in ["", "not xml at all", "<tt><body><div/></body></tt>", "<pfoobar/>"] {
            let tl = LyricTimeline::parse(doc);
            assert!(tl.is_empty(), "{doc:?} must parse to nothing");
            assert_eq!(tl.line_at(0), None);
        }
    }
}
