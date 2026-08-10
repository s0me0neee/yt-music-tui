//! Lyrics lookup against [lrclib.net](https://lrclib.net).
//!
//! [`lrclib`] is the transport; this module is the policy. No single LRCLIB
//! endpoint does what we need — `/get` takes a duration but returns one result,
//! `/search` returns many but ignores duration — so [`LyricsService::best_for`]
//! layers the two and [`rank`] does duration-proximity scoring client-side.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use lrclib::{LrcError, LrcLib, Lyrics, parse_lrc};

/// Re-exported so consumers don't need `lrclib` as a direct dependency.
pub use lrclib::{LyricLine, active_index, next_boundary};

use crate::error::Result;
use crate::library::Track;

/// How far a record's length may differ from the track's before its *timing*
/// stops being trustworthy. A few seconds of slack covers the usual difference
/// between a YouTube upload and a release master; past that, synced lyrics
/// drift visibly out of time.
///
/// Records outside this are not discarded — they are demoted to plain text
/// (see [`TrackLyrics::demote_to_plain`]), because the words are still right
/// even when the timings aren't.
const SYNC_DURATION_DELTA: f64 = 5.0;

/// Past this, the record is a different song rather than a different edit, and
/// is dropped outright. Generous enough for a cover or live take, which can
/// legitimately run a good deal longer or shorter than the original.
const MAX_DURATION_DELTA: f64 = 30.0;

/// A synced record this close to the track's length is a clear winner: the
/// search stops there rather than broadening, and the exact-lookup hit is
/// accepted without consulting search at all.
///
/// Sub-second deliberately. Both lrclib and YouTube report whole seconds for
/// most tracks, so a genuine match lands on zero — and anything a second out
/// must still compete, because an exactly-matching record often exists under
/// slightly different metadata and would otherwise never be reached.
const DECISIVE_DURATION_DELTA: f64 = 0.5;

/// Words that mark a bracketed group as production decoration rather than part
/// of the song's name. `(Remix)` and `(self cover)` are deliberately absent —
/// those denote genuinely different recordings.
const NOISE_WORDS: &[&str] = &[
    "official",
    "video",
    "audio",
    "mv",
    "m/v",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "remaster",
    "remastered",
    "hd",
    "hq",
    "4k",
    "full version",
    "explicit",
    "clean",
    "feat",
    "feat.",
    "ft",
    "ft.",
    "featuring",
];

/// Markers identifying a cover upload. A cover carries the *original's*
/// lyrics, so lookup wants the original song's name — and must not constrain
/// by artist, since whoever covered it will never match the lyrics record.
const COVER_WORDS: &[&str] = &[
    "歌ってみた",
    "唄ってみた",
    "うたってみた",
    "カバー",
    "cover",
    "covered",
];

/// Phrases introducing the *performer of this rendition* — everything from
/// here on is credit, not title. `ver.` is the usual Japanese form
/// (`ダーリン ver.わかばやし`). These also mark the title as a rendition.
const RENDITION_CREDITS: &[&str] = &[
    "covered by",
    "cover by",
    "cover:",
    "covered:",
    "ver.",
    "ver:",
];

/// Guest-artist credits. Also trimmed, but they describe the *same* recording,
/// so unlike [`RENDITION_CREDITS`] they don't make a title a cover. The
/// bracketed form is handled by [`NOISE_WORDS`].
const GUEST_CREDITS: &[&str] = &["feat.", "feat ", "ft.", "featuring "];

/// Finds `needle` in `hay` (both lowercased) only where it begins at a word
/// boundary, so `ver.` matches in `ダーリン ver.わかばやし` and `ダーリンver.`
/// but not inside `Cover.`.
fn find_at_boundary(hay: &str, needle: &str) -> Option<usize> {
    // A needle that opens with punctuation or a space carries its own
    // delimiter, so the preceding character says nothing.
    let check_prev = needle
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());

    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        // ASCII letters/digits before the marker mean we're mid-word. CJK is
        // allowed, since Japanese titles run the marker straight on.
        let mid_word = hay[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if !check_prev || !mid_word {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

/// Whether `haystack` mentions `word`, using the same ASCII-boundary /
/// CJK-substring rule as [`strip_bracketed`].
fn mentions(haystack_lower: &str, word: &str) -> bool {
    if word.is_ascii() {
        find_at_boundary(haystack_lower, word).is_some()
    } else {
        haystack_lower.contains(word)
    }
}

/// Whether the title announces itself as a cover or alternate rendition.
///
/// This gates the riskier trimming: `ドゥーマー by 花譜` should lose its
/// credit, but `Stand By Me` must not lose half its name.
fn has_cover_marker(title: &str) -> bool {
    let lower = title.to_lowercase();
    // Guest credits are deliberately excluded: `Stand By Me feat. X` is not a
    // cover, and must not license cutting at `by`.
    COVER_WORDS.iter().any(|w| mentions(&lower, w))
        || RENDITION_CREDITS.iter().any(|w| mentions(&lower, w))
}

/// Removes bracketed groups whose contents mention one of `keywords`.
fn strip_bracketed(title: &str, keywords: &[&str]) -> String {
    let mut out = String::with_capacity(title.len());
    let mut rest = title;

    while let Some(open) = rest.find(['(', '[', '【', '「', '『']) {
        let open_char = rest[open..].chars().next().unwrap();
        let close_char = match open_char {
            '(' => ')',
            '[' => ']',
            '【' => '】',
            '「' => '」',
            _ => '』',
        };
        let Some(close_rel) = rest[open..].find(close_char) else {
            break; // Unbalanced — leave the remainder alone.
        };
        let close = open + close_rel;
        let inner = rest[open + open_char.len_utf8()..close].to_lowercase();

        let matches = keywords.iter().any(|w| {
            if w.is_ascii() {
                // Token match, so "hd" doesn't fire inside "shd".
                inner
                    .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '/')
                    .any(|token| token == *w)
            } else {
                // CJK has no word boundaries to split on.
                inner.contains(w)
            }
        });

        let after_close = close + close_char.len_utf8();
        out.push_str(&rest[..open]);
        if !matches {
            // `..=close` would slice mid-character: `】` and `」` are 3 bytes,
            // so the inclusive range ends inside the closing bracket.
            out.push_str(&rest[open..after_close]);
        }
        rest = &rest[after_close..];
    }
    out.push_str(rest);

    // Collapse the whitespace that removing a group leaves behind.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strips YouTube-style decoration from a track title.
///
/// YouTube Music titles routinely carry `(Official Video)`, `[MV]` or
/// `(feat. X)`, none of which LRCLIB knows about — and because LRCLIB matches
/// the track name as text, one such suffix takes the search from several hits
/// to zero. Only groups containing a [`NOISE_WORDS`] entry are removed, so
/// meaningful qualifiers like `(Remix)` survive.
pub fn strip_title_noise(title: &str) -> String {
    strip_bracketed(title, NOISE_WORDS)
}

/// Field separators YouTube uses to append an alias, an artist or a credit.
const FIELD_SEPARATORS: &[&str] = &[" - ", " – ", " — ", " / ", "／", " ・ "];

/// Splits `s` at the first field separator, into what precedes and follows it.
fn split_field(s: &str) -> Option<(&str, &str)> {
    FIELD_SEPARATORS
        .iter()
        .filter_map(|sep| s.find(sep).map(|at| (at, sep)))
        .min_by_key(|&(at, _)| at)
        .map(|(at, sep)| (&s[..at], &s[at + sep.len()..]))
}

/// Reduces a title to just the song's name, discarding cover credits.
///
/// Cover uploads are titled like `【歌ってみた】人マニア / covered by ヰ世界情緒`.
/// LRCLIB has the *original*, so the lyrics are found by song name alone:
/// searching the full title returns nothing, and so does constraining by the
/// coverer.
///
/// `artist` disambiguates the `A - B` layout, which YouTube uses for both
/// `song - alias` and `artist - song`; pass `""` when it is unknown.
///
/// Deliberately more aggressive than [`strip_title_noise`], so it is only used
/// by the late, broadening steps of the search ladder.
pub fn song_title_only(title: &str, artist: &str) -> String {
    // A bare `by <name>` is only a credit when the title already says it's a
    // cover — `【歌ってみた】ドゥーマー by 花譜` versus `Stand By Me`.
    let mut credits: Vec<&str> = [RENDITION_CREDITS, GUEST_CREDITS].concat();
    if has_cover_marker(title) {
        credits.push(" by ");
    }

    let mut out = strip_bracketed(title, &[NOISE_WORDS, COVER_WORDS].concat());

    // Cut at an explicit cover credit. The index comes from a lowercased copy,
    // and lowercasing can change byte length for a few characters, so only
    // trust it if it still lands on a boundary of the original.
    let lower = out.to_lowercase();
    if let Some(cut) = credits
        .iter()
        .filter_map(|m| find_at_boundary(&lower, m))
        .min()
        .filter(|&cut| out.is_char_boundary(cut))
    {
        out.truncate(cut);
        // Drop the separator that introduced the credit.
        out = out
            .trim_end_matches([' ', '/', '／', '-', '–', '—', '・'])
            .to_string();
    }

    // `Artist - Song`, the standard layout for uploads outside YouTube's Topic
    // channels. The rule below keeps the *leading* field, which here is the
    // artist — so `Lorde - Ribs` would search lrclib for "Lorde". When the
    // leading field is the credited artist, the song is what follows it.
    if let Some((lead, rest)) = split_field(&out)
        && !rest.trim().is_empty()
        && credits_the_artist(lead, artist)
    {
        out = rest.trim().to_string();
    }

    // Drop a trailing alias or credit field. YouTube Music appends an English
    // or romanised alias to non-English titles — `法螺話 - Tall Story`,
    // `キャラクターT - Character T` — and lrclib usually stores just the
    // original, so the combined form matches nothing. Japanese uploads also use
    // `Song / Artist`.
    if let Some((lead, _)) = split_field(&out) {
        out = lead.to_string();
    }

    out.trim().to_string()
}

/// Whether `field` is the track's credited artist rather than part of its name.
///
/// Both the full credit and its primary form count, since a title carries
/// whichever the uploader felt like using.
fn credits_the_artist(field: &str, artist: &str) -> bool {
    let field = field.trim().to_lowercase();
    if field.is_empty() || artist.trim().is_empty() {
        return false;
    }
    field == artist.trim().to_lowercase() || field == primary_artist(artist).to_lowercase()
}

/// Normalises an artist for searching: drops YouTube's auto-channel `- Topic`
/// suffix and keeps only the first credited artist.
///
/// LRCLIB stores one artist string per record, so a joined list like
/// `"理芽, Guiano"` frequently matches nothing even when the track is present.
pub fn primary_artist(artist: &str) -> String {
    let artist = artist
        .strip_suffix(" - Topic")
        .or_else(|| artist.strip_suffix(" - topic"))
        .unwrap_or(artist);
    // `、` is the ideographic comma, which Japanese credits join with.
    // `&` deliberately isn't a separator — it is part of plenty of band names.
    artist
        .split(&[',', ';', '、'][..])
        .next()
        .unwrap_or(artist)
        .trim()
        .to_string()
}

/// One LRCLIB query in the broadening ladder.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Attempt {
    Meta {
        track: String,
        artist: String,
        album: String,
    },
    FreeText(String),
}

// ── query ────────────────────────────────────────────────────────────────────

/// What we know about the track we want lyrics for.
#[derive(Debug, Clone)]
pub struct LyricsQuery {
    pub title: String,
    pub artist: String,
    /// Empty when unknown — callers omit it from the request rather than
    /// sending a filter that matches nothing.
    pub album: String,
    pub duration: Option<f64>,
}

impl LyricsQuery {
    /// Returns `None` when the track has no title, i.e. nothing to search on.
    pub fn from_track(track: &Track) -> Option<Self> {
        Some(Self {
            title: track.title.clone()?,
            artist: track.artist_names(),
            album: track
                .album
                .as_ref()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            duration: track.duration_seconds.map(f64::from),
        })
    }

    /// Progressively looser LRCLIB queries, most precise first.
    ///
    /// YouTube Music metadata rarely matches LRCLIB exactly: album names differ
    /// (and LRCLIB treats `album_name` as a hard filter, so a wrong one returns
    /// *nothing*), artists arrive joined or suffixed `- Topic`, and titles carry
    /// production decoration. Starting precise keeps good matches ranked first;
    /// broadening is what stops a decorated title reporting "no lyrics".
    ///
    /// Duplicate steps are collapsed, so a track with clean metadata costs one
    /// request.
    fn search_ladder(&self) -> Vec<Attempt> {
        let clean = strip_title_noise(&self.title);
        let primary = primary_artist(&self.artist);
        let song = song_title_only(&self.title, &self.artist);

        let mut ladder = vec![
            // Everything we know.
            Attempt::Meta {
                track: self.title.clone(),
                artist: self.artist.clone(),
                album: self.album.clone(),
            },
            // Album dropped — the most common single cause of a false empty.
            Attempt::Meta {
                track: self.title.clone(),
                artist: self.artist.clone(),
                album: String::new(),
            },
            // Undecorated title, single artist.
            Attempt::Meta {
                track: clean.clone(),
                artist: primary.clone(),
                album: String::new(),
            },
            // Title only — right song, possibly a different credit.
            Attempt::Meta {
                track: clean.clone(),
                artist: String::new(),
                album: String::new(),
            },
            // Bare song name, still credited.
            Attempt::Meta {
                track: song.clone(),
                artist: primary.clone(),
                album: String::new(),
            },
            // Bare song name, no artist. This is what finds a cover: lrclib
            // holds the original, whose credited artist is not the coverer.
            Attempt::Meta {
                track: song.clone(),
                artist: String::new(),
                album: String::new(),
            },
            // Free text, which matches across fields.
            Attempt::FreeText(format!("{clean} {primary}").trim().to_string()),
            Attempt::FreeText(clean),
            Attempt::FreeText(song),
        ];
        // Well-tagged tracks collapse to a single request. `Vec::dedup` only
        // removes *consecutive* duplicates, and the rungs interleave, so an
        // identical query could reappear later and cost a second request.
        let mut seen = HashSet::new();
        ladder.retain(|a| seen.insert(a.clone()));
        ladder
    }
}

// ── results ──────────────────────────────────────────────────────────────────

/// The lyric content of one LRCLIB record.
#[derive(Debug, Clone, PartialEq)]
pub enum LyricsKind {
    /// Timestamped lines that follow playback.
    Synced(Vec<LyricLine>),
    /// Plain text — no timing information available.
    Plain(Vec<String>),
    Instrumental,
}

/// One usable LRCLIB record: its metadata plus parsed content.
#[derive(Debug, Clone)]
pub struct TrackLyrics {
    pub id: u64,
    pub track_name: String,
    pub artist_name: String,
    pub album_name: String,
    /// `None` when lrclib has no duration for the record.
    pub duration: Option<f64>,
    /// Set when the record's length is too far from the track's for its
    /// timings to be trusted, or when lrclib has no duration to check. The
    /// lyrics are still offered — as plain text — since the words are the
    /// song's even if the timings belong to a different recording.
    pub timing_mismatch: bool,
    /// Position in lrclib's own result ordering, lowest first.
    ///
    /// lrclib returns search hits ranked by relevance — exact title matches
    /// ahead of decorated variants — and that ordering is stable across
    /// requests. It is the final tiebreak, so two records our own signals
    /// can't separate fall back to lrclib's judgement. Recorded explicitly
    /// rather than relying on the sort being stable, since results are merged
    /// from several queries.
    ///
    /// The sequence spans the whole ladder, so an earlier (more precise) rung
    /// always ranks ahead of a later one. Zero is reserved for the `/get` hit:
    /// no search result is a more precise match than an exact lookup.
    pub relevance: usize,
    pub kind: LyricsKind,
}

impl TrackLyrics {
    /// How far this record's length is from `want`, in seconds. `None` when
    /// either side is unknown.
    pub fn duration_delta(&self, want: Option<f64>) -> Option<f64> {
        Some((self.duration? - want?).abs())
    }

    pub fn is_synced(&self) -> bool {
        matches!(self.kind, LyricsKind::Synced(_))
    }

    /// Drops the timings, keeping the words.
    ///
    /// Used when the record's length says its timings belong to a different
    /// recording: scrolling those against this track would simply be wrong,
    /// but the lyrics themselves are still the ones being sung.
    fn demote_to_plain(&mut self) {
        self.timing_mismatch = true;
        if let LyricsKind::Synced(lines) = &self.kind {
            self.kind = LyricsKind::Plain(lines.iter().map(|l| l.text.clone()).collect());
        }
    }

    /// How many lines of lyrics this record carries, timed or not.
    pub fn line_count(&self) -> usize {
        match &self.kind {
            LyricsKind::Synced(lines) => lines.len(),
            LyricsKind::Plain(lines) => lines.len(),
            LyricsKind::Instrumental => 0,
        }
    }

    pub fn synced_lines(&self) -> Option<&[LyricLine]> {
        match &self.kind {
            LyricsKind::Synced(lines) => Some(lines),
            _ => None,
        }
    }

    /// A fingerprint of what this record actually puts on screen.
    ///
    /// lrclib is full of the same lyrics re-uploaded under a different album,
    /// and the picker shows every copy — for some tracks *every* result is the
    /// same words and the same timings under six different album names.
    ///
    /// The timings are part of the identity, deliberately. Two records whose
    /// words agree but whose timings differ are a real choice, and which of
    /// them scrolls correctly is the whole reason the picker exists; only when
    /// both agree is there nothing left to choose between.
    fn content_fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match &self.kind {
            LyricsKind::Synced(lines) => {
                0u8.hash(&mut h);
                for l in lines {
                    // Centiseconds — LRC's own precision, so two copies of one
                    // file agree exactly without depending on float equality.
                    ((l.at * 100.0).round() as i64).hash(&mut h);
                    l.text.hash(&mut h);
                }
            }
            LyricsKind::Plain(lines) => {
                1u8.hash(&mut h);
                for l in lines {
                    l.trim().hash(&mut h);
                }
            }
            // No content to tell apart: one "instrumental" row says everything
            // a dozen of them would.
            LyricsKind::Instrumental => 2u8.hash(&mut h),
        }
        h.finish()
    }

    /// Converts a raw record, or `None` if it carries no usable content.
    ///
    /// Synced text that is present but unparseable falls through to plain —
    /// both that and `Some("")` occur in LRCLIB's data.
    fn from_record(l: Lyrics) -> Option<Self> {
        let kind = if l.instrumental {
            LyricsKind::Instrumental
        } else {
            let synced = l
                .synced_lyrics
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(parse_lrc)
                .filter(|lines| !lines.is_empty());

            match synced {
                Some(lines) => LyricsKind::Synced(lines),
                None => {
                    let plain = l.plain_lyrics.as_deref().filter(|s| !s.trim().is_empty())?;
                    LyricsKind::Plain(plain.lines().map(str::to_string).collect())
                }
            }
        };

        Some(Self {
            id: l.id,
            timing_mismatch: false,
            // Search results are re-stamped by `with_relevance` once their
            // position is known; a `/get` hit keeps zero, which outranks them.
            relevance: 0,
            track_name: l.track_name,
            artist_name: l.artist_name,
            album_name: l.album_name,
            duration: l.duration,
            kind,
        })
    }
}

// ── ranking ──────────────────────────────────────────────────────────────────

/// Stamps lrclib's ordering onto a batch of records, continuing from `from`.
fn with_relevance(records: Vec<TrackLyrics>, from: usize) -> Vec<TrackLyrics> {
    records
        .into_iter()
        .enumerate()
        .map(|(i, mut c)| {
            c.relevance = from + i;
            c
        })
        .collect()
}

/// Merges one rung's response into `pool`, skipping records already held, and
/// returns the next free relevance slot.
///
/// Every record parsed consumes a slot, whether or not it survives
/// de-duplication. Counting only the *kept* ones let the following rung reuse
/// numbers this one had already handed out, so a broad rung's record could be
/// stamped more relevant than a precise rung's — the exact inversion the
/// counter exists to prevent.
fn merge_rung(
    pool: &mut Vec<TrackLyrics>,
    seen: &mut HashSet<u64>,
    raw: Vec<Lyrics>,
    next_relevance: usize,
) -> usize {
    let batch = with_relevance(
        raw.into_iter()
            .filter_map(TrackLyrics::from_record)
            .collect(),
        next_relevance,
    );
    let next = next_relevance + batch.len();
    for found in batch {
        if seen.insert(found.id) {
            pool.push(found);
        }
    }
    next
}

/// Orders candidates best-first: synced before plain, then closest duration,
/// then artist/title agreement, then LRCLIB's own relevance order.
///
/// Records more than [`MAX_DURATION_DELTA`] from a known duration are dropped
/// outright — at that distance they are a different song, and offering the
/// wrong lyrics is worse than offering none.
pub fn rank(mut items: Vec<TrackLyrics>, q: &LyricsQuery) -> Vec<TrackLyrics> {
    if q.duration.is_some() {
        // Far enough out and it's a different song, not a different edit.
        items.retain(|c| {
            c.duration_delta(q.duration)
                .is_none_or(|d| d <= MAX_DURATION_DELTA)
        });
        // Close enough to be the same song but too far for the timings to line
        // up — keep the words, drop the clock. A cover running 13s longer than
        // the original is the usual case.
        for c in &mut items {
            if !c
                .duration_delta(q.duration)
                .is_some_and(|d| d <= SYNC_DURATION_DELTA)
            {
                c.demote_to_plain();
            }
        }
    }

    // Compare against the normalised forms: when the match came from a
    // broadened query, the raw title still carries the decoration that made the
    // precise query fail, so it would never compare equal.
    let title = strip_title_noise(&q.title).to_lowercase();
    let artist = primary_artist(&q.artist).to_lowercase();
    let typical = typical_line_count(&items, q.duration);

    // Stable, so equal keys preserve LRCLIB's relevance ordering.
    items.sort_by(|a, b| {
        let key = |c: &TrackLyrics| {
            (
                // Synced first — an unsynced match is a worse experience than a
                // slightly mistimed synced one.
                !c.is_synced(),
                // Then whether the record is even this artist's. Above length
                // because a record for a *different song of the same name* can
                // match the duration exactly, and several did: a track called
                // "Ride It" by another artist beat the right one by 0.2s, and
                // "Do Better" matched a K-pop group over Feint.
                !credits_artist(&c.artist_name.to_lowercase(), &artist),
                // Then whether it looks like a fragment of the song rather than
                // the song. Above length for the same reason: a stub can carry
                // a perfect duration and half the words.
                is_stub(c, q.duration, typical),
                // Then closest length. Rounded to the second so a 0.4s
                // difference doesn't outweigh a title match. Records with no
                // duration sort last rather than being treated as a perfect
                // match, which is what `unwrap_or(0.0)` used to do.
                c.duration_delta(q.duration)
                    .map_or(f64::INFINITY, |d| d.round()),
                !c.track_name.to_lowercase().eq(&title),
                // Last word goes to lrclib's own relevance ranking.
                c.relevance,
            )
        };
        let (a_sync, a_art, a_stub, a_delta, a_tit, a_rel) = key(a);
        let (b_sync, b_art, b_stub, b_delta, b_tit, b_rel) = key(b);
        a_sync
            .cmp(&b_sync)
            .then(a_art.cmp(&b_art))
            .then(a_stub.cmp(&b_stub))
            .then(a_delta.total_cmp(&b_delta))
            .then(a_tit.cmp(&b_tit))
            .then(a_rel.cmp(&b_rel))
    });

    items
}

/// A record carrying fewer than this share of the typical line count is a
/// fragment — a first verse, a chorus, an abandoned transcription.
///
/// Deliberately well below half. lrclib records of the same song vary a lot in
/// how they treat repeats and backing vocals, and the gaps that matter are not
/// subtle: the records corrected by hand ran 1.8x to 2.6x the length of what
/// was picked automatically.
const STUB_LINE_RATIO: f64 = 0.6;

/// Whether `c` is close enough in length to be a transcription of this exact
/// recording, and so a fair yardstick for how long its lyrics should be.
///
/// The ladder's broad rungs deliberately return records for *other songs that
/// share a title*, and their line counts say nothing about this one. Measured
/// against the whole pool the rule below actively misfires: for a cover of
/// `GURU` the pool held four unrelated songs of that name and the median came
/// out at 54 lines, making the correct 24-line record look like a fragment.
fn is_peer(c: &TrackLyrics, want: Option<f64>) -> bool {
    !matches!(c.kind, LyricsKind::Instrumental)
        && c.duration_delta(want)
            .is_some_and(|d| d <= SYNC_DURATION_DELTA)
}

/// The line count a complete record of this song has, judged from the peers.
/// `None` when there are too few to tell an outlier from an ordinary spread.
///
/// Counted once per *distinct* set of lyrics: two thirds of what lrclib returns
/// for a popular track is the same record re-uploaded, so counting every copy
/// would let a fragment uploaded a dozen times set the standard it is measured
/// against.
fn typical_line_count(items: &[TrackLyrics], want: Option<f64>) -> Option<usize> {
    let mut seen = HashSet::new();
    let mut counts: Vec<usize> = items
        .iter()
        .filter(|c| is_peer(c, want))
        .filter(|c| seen.insert(c.content_fingerprint()))
        .map(TrackLyrics::line_count)
        .collect();

    if counts.len() < 3 {
        return None;
    }
    counts.sort_unstable();
    Some(counts[counts.len() / 2])
}

/// Whether `c` holds markedly less of the song than its peers do.
fn is_stub(c: &TrackLyrics, want: Option<f64>, typical: Option<usize>) -> bool {
    match typical {
        Some(typical) if is_peer(c, want) => {
            (c.line_count() as f64) < STUB_LINE_RATIO * typical as f64
        }
        _ => false,
    }
}

/// Whether a record credits `artist`, which is already lowercased and reduced
/// by [`primary_artist`].
///
/// Bounded on both sides so `Ado` doesn't match `Adore`, and so a promoted
/// artist key can't be won by an accident of spelling. Only ASCII letters and
/// digits count as continuing a word: Japanese credits run names straight into
/// punctuation and each other, and demanding a break there would match nothing.
fn credits_artist(record_lower: &str, artist: &str) -> bool {
    if artist.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(rel) = record_lower[from..].find(artist) {
        let at = from + rel;
        let end = at + artist.len();
        let before_ok = record_lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_ok = record_lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Drops every record whose lyrics duplicate one already in the list.
///
/// Applied to a ranked list, so the copy that survives each group is the
/// best-ranked one and the order of what remains is untouched.
///
/// This is not a cosmetic tidy-up. Two thirds of what the ladder reaches for a
/// popular track is the same file under a different album name, which buries
/// the records that genuinely differ and turns choosing into a scroll through
/// twenty identical rows.
/// `keep` names the record currently on screen. It survives its group even
/// when a better-ranked copy exists, because the picker has to be able to mark
/// which one is in use — you cannot see what you are choosing away from, or
/// notice you have re-picked what you already have, if it isn't listed.
pub fn dedupe_by_content(items: Vec<TrackLyrics>, keep: Option<u64>) -> Vec<TrackLyrics> {
    let protected = keep.and_then(|id| {
        items
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.content_fingerprint())
    });

    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|c| {
            let fingerprint = c.content_fingerprint();
            if protected == Some(fingerprint) {
                // Exactly one record represents this group, and it is the one
                // the user is looking at.
                return Some(c.id) == keep;
            }
            seen.insert(fingerprint)
        })
        .collect()
}

/// Orders candidates and then drops duplicated lyrics — what every caller
/// wanting a final, user-facing list needs.
fn rank_and_dedupe(
    items: Vec<TrackLyrics>,
    q: &LyricsQuery,
    keep: Option<u64>,
) -> Vec<TrackLyrics> {
    dedupe_by_content(rank(items, q), keep)
}

// ── service ──────────────────────────────────────────────────────────────────

/// How many times a transient lrclib failure is repeated before giving up.
///
/// One, not several. A dropped connection or a rate-limit tick clears on the
/// second try, while a server that is genuinely down would otherwise multiply
/// its timeout across every rung of the ladder — nine rungs at ten seconds is
/// already the worst case, and doubling that is felt. Beyond one retry the
/// lyrics panel says so and `r` runs the lookup again on demand.
const MAX_RETRIES: u32 = 1;

/// How long to wait before repeating a request. Long enough to outlast a blip,
/// short enough to go unnoticed in a background fetch.
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// Repeats `attempt` while it fails for a reason that might not recur.
///
/// Which failures those are is [`LrcError::is_transient`]'s call — this is the
/// policy over it. A settled answer, including a 404, returns immediately.
async fn with_retry<F, Fut, T>(what: &str, mut attempt: F) -> std::result::Result<T, LrcError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, LrcError>>,
{
    let mut backoff = RETRY_BACKOFF;
    for _ in 0..MAX_RETRIES {
        match attempt().await {
            Err(e) if e.is_transient() => {
                log::debug!("lyrics: {what} failed ({e}) — retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            settled => return settled,
        }
    }
    attempt().await
}

pub struct LyricsService {
    client: LrcLib,
}

impl Default for LyricsService {
    fn default() -> Self {
        Self::new()
    }
}

impl LyricsService {
    /// # Panics
    /// See [`LrcLib::new`] — build this before taking over the terminal.
    pub fn new() -> Self {
        Self {
            client: LrcLib::new(),
        }
    }

    /// Re-fetches a specific record. `Ok(None)` when it no longer exists or
    /// carries no usable content.
    pub async fn by_id(&self, id: u64) -> Result<Option<TrackLyrics>> {
        match with_retry("lookup by id", || self.client.get_by_id(id)).await {
            Ok(l) => Ok(TrackLyrics::from_record(l)),
            Err(LrcError::Api {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The best lyrics for `q`, honouring a previously-chosen `override_id`.
    ///
    /// `Ok(None)` means LRCLIB simply has nothing — a normal outcome, not an
    /// error. Prefers synced over plain, per the feature's stated rule.
    pub async fn best_for(
        &self,
        q: &LyricsQuery,
        override_id: Option<u64>,
    ) -> Result<Option<TrackLyrics>> {
        // 1. An explicit user choice wins outright. If it fails to resolve we
        //    fall through to automatic matching rather than erroring — but we
        //    never clear the override here, so a network blip can't silently
        //    discard the user's decision.
        if let Some(id) = override_id {
            match self.by_id(id).await {
                Ok(Some(found)) => return Ok(Some(found)),
                Ok(None) => {
                    log::warn!("lyrics override #{id} no longer resolves — using automatic")
                }
                Err(e) => log::warn!("lyrics override #{id} failed ({e}) — using automatic"),
            }
        }

        // 2. Exact lookup. A synced hit whose length really matches ends it in
        //    one request; anything looser is only held as a candidate, so a
        //    closer-timed record in the search results can still win.
        let mut exact: Option<TrackLyrics> = None;
        let mut plain_fallback: Option<TrackLyrics> = None;
        if let Some(duration) = q.duration {
            match with_retry("exact lookup", || {
                self.client.get(&q.title, &q.artist, &q.album, duration)
            })
            .await
            {
                Ok(l) => {
                    if let Some(found) = TrackLyrics::from_record(l) {
                        let close = found
                            .duration_delta(q.duration)
                            .is_some_and(|d| d <= DECISIVE_DURATION_DELTA);
                        if found.is_synced() && close {
                            return Ok(Some(found));
                        }
                        if found.is_synced() {
                            exact = Some(found);
                        } else {
                            // Hold it, but keep looking for a synced alternative.
                            plain_fallback = Some(found);
                        }
                    }
                }
                Err(LrcError::Api {
                    status_code: 404, ..
                }) => {}
                Err(e) => log::warn!("lrclib get failed ({e}) — falling back to search"),
            }
        }

        // 3. Broaden to search.
        //
        // Errors propagate rather than being swallowed into an empty list: a
        // network failure and "lrclib genuinely has nothing" are different
        // things, and the UI shows them differently.
        let mut found = match self.search_ladder(q, true, None).await {
            Ok(found) => found,
            // The ladder failed for real, retries included. Anything the exact
            // lookup is still holding beats reporting nothing: it is the same
            // record the search would have had to beat, and a synced hit a
            // second or two out is exactly what the user wants on screen. The
            // search that would have improved on it is a keypress away.
            Err(e) => match exact.or(plain_fallback) {
                Some(held) => {
                    log::warn!("lyrics search failed ({e}) — using the exact-lookup match");
                    return Ok(Some(held));
                }
                None => return Err(e),
            },
        };

        // Fold the exact-lookup hit into the pool and re-rank, so whichever
        // record's length is closest to the track wins — that hit is often not
        // the best-timed one available.
        if let Some(exact) = exact {
            if !found.iter().any(|c| c.id == exact.id) {
                found.push(exact);
            }
            found = rank_and_dedupe(found, q, None);
        }

        if !found.is_empty() {
            let best = found.remove(0);
            // Only prefer a search hit over an exact plain hit if it's synced.
            if best.is_synced() || plain_fallback.is_none() {
                return Ok(Some(best));
            }
        }

        Ok(plain_fallback)
    }

    /// Runs one rung of the ladder.
    async fn run(&self, attempt: &Attempt) -> std::result::Result<Vec<Lyrics>, LrcError> {
        match attempt {
            Attempt::Meta {
                track,
                artist,
                album,
            } => {
                with_retry("metadata search", || {
                    self.client.search_by_meta(track, artist, album)
                })
                .await
            }
            Attempt::FreeText(query) => {
                with_retry("free-text search", || self.client.search(query)).await
            }
        }
    }

    /// Whether `found` settles the search outright: synced, and its length
    /// matches the track closely enough that nothing better can exist.
    fn is_decisive(found: &TrackLyrics, q: &LyricsQuery) -> bool {
        found.is_synced()
            && found
                .duration_delta(q.duration)
                .is_some_and(|d| d <= DECISIVE_DURATION_DELTA)
    }

    /// Walks the ladder, merging each rung's results into one ranked pool.
    ///
    /// With `stop_early`, the walk ends as soon as the pool holds a decisive
    /// match — so a well-tagged track still costs one request. Without it,
    /// every rung runs and the caller sees everything reachable.
    ///
    /// Merging rather than returning the first rung that matched is what makes
    /// the best record win: a precise query often returns a near-miss (a
    /// transcription a second out, or an unsynced upload) while the exact one
    /// sits under looser metadata — a plain `Sway` where the track is
    /// `Sway (feat. Nevve)` — and is only reachable from a broader rung.
    async fn search_ladder(
        &self,
        q: &LyricsQuery,
        stop_early: bool,
        keep: Option<u64>,
    ) -> Result<Vec<TrackLyrics>> {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut pool: Vec<TrackLyrics> = Vec::new();
        let mut last_err = None;
        // Spans the whole ladder rather than restarting per rung, so a later
        // rung can never be stamped more relevant than an earlier one. Starts
        // at 1: zero belongs to the `/get` hit `best_for` may fold in.
        let mut next_relevance = 1;

        for attempt in q.search_ladder() {
            if matches!(&attempt, Attempt::FreeText(s) if s.is_empty()) {
                continue;
            }
            match self.run(&attempt).await {
                Ok(raw) => {
                    // A response whose records all lack lyrics still counts as
                    // a miss — keep broadening rather than reporting "no lyrics
                    // found" as an early `raw.is_empty()` check would.
                    next_relevance = merge_rung(&mut pool, &mut seen, raw, next_relevance);

                    // A decisive record is synced and within half a second, so
                    // `rank` can neither drop nor demote it — and anything rank
                    // would place above it is synced and inside half a second
                    // too, hence decisive itself. Scanning the pool therefore
                    // answers exactly what ranking it would, without cloning
                    // every candidate's lyrics on every rung.
                    if stop_early
                        && let Some(id) =
                            pool.iter().find(|c| Self::is_decisive(c, q)).map(|c| c.id)
                    {
                        log::debug!("lyrics: decisive match #{id} on {attempt:?}");
                        return Ok(rank_and_dedupe(pool, q, keep));
                    }
                }
                // One failing rung shouldn't abort the ladder — a later,
                // simpler query may well succeed.
                Err(e) => {
                    log::warn!("lyrics: {attempt:?} failed: {e}");
                    last_err = Some(e);
                }
            }
        }

        match last_err {
            // Every rung failed and nothing was found: an error, not an absence.
            Some(e) if pool.is_empty() => Err(e.into()),
            _ => {
                log::debug!("lyrics: {} candidates across the ladder", pool.len());
                Ok(rank_and_dedupe(pool, q, keep))
            }
        }
    }

    /// Every candidate the ladder can reach, de-duplicated and ranked, always
    /// including the record `on_screen` names.
    ///
    /// Unlike the automatic match this never stops early, so the picker offers
    /// everything a manual lrclib search would turn up. It is only invoked when
    /// the user presses `c`, so the extra requests are paid for deliberately.
    ///
    /// `on_screen` is the record the lyrics panel is currently showing. It is
    /// guaranteed a row of its own — it survives de-duplication, and is fetched
    /// by id when the ladder can't reach it at all. Both happen routinely: the
    /// automatic match is often the exact `/get` lookup's hit, which never goes
    /// through search, and a previous manual choice resolves the same way. A
    /// picker that silently omitted it would offer no way to tell which lyrics
    /// are already in use, or to notice that a row is the one you have.
    pub async fn candidates(
        &self,
        q: &LyricsQuery,
        on_screen: Option<u64>,
    ) -> Result<Vec<TrackLyrics>> {
        let mut found = self.search_ladder(q, false, on_screen).await?;

        if let Some(id) = on_screen
            && !found.iter().any(|c| c.id == id)
        {
            match self.by_id(id).await {
                Ok(Some(current)) => {
                    found.push(current);
                    found = rank_and_dedupe(found, q, on_screen);
                }
                // Not fatal: the list is still useful, it just can't mark the
                // record in use.
                Ok(None) => log::warn!("lyrics: record #{id} on screen no longer resolves"),
                Err(e) => log::warn!("lyrics: could not fetch the record on screen #{id}: {e}"),
            }
        }

        Ok(found)
    }
}

// ── background fetching ──────────────────────────────────────────────────────

/// A completed background lyrics fetch.
///
/// `video_id` is carried so the UI can key its cache and discard results for a
/// track that is no longer on screen. Errors are pre-stringified: the UI only
/// ever needs display text, and this keeps the message trivially `Send`.
pub enum LyricsMsg {
    Best {
        video_id: String,
        result: std::result::Result<Option<TrackLyrics>, String>,
    },
    Choices {
        video_id: String,
        result: std::result::Result<Vec<TrackLyrics>, String>,
    },
}

/// Looks up the best lyrics for one track in the background.
pub fn spawn_best(
    handle: &tokio::runtime::Handle,
    svc: Arc<LyricsService>,
    video_id: String,
    query: LyricsQuery,
    override_id: Option<u64>,
    tx: Sender<LyricsMsg>,
) {
    handle.spawn(async move {
        let result = svc
            .best_for(&query, override_id)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(LyricsMsg::Best { video_id, result });
    });
}

/// Fetches the candidate list for the picker in the background.
pub fn spawn_choices(
    handle: &tokio::runtime::Handle,
    svc: Arc<LyricsService>,
    video_id: String,
    query: LyricsQuery,
    on_screen: Option<u64>,
    tx: Sender<LyricsMsg>,
) {
    handle.spawn(async move {
        let result = svc
            .candidates(&query, on_screen)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(LyricsMsg::Choices { video_id, result });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(duration: Option<f64>) -> LyricsQuery {
        LyricsQuery {
            title: "Echo".into(),
            artist: "Crusher-P".into(),
            album: String::new(),
            duration,
        }
    }

    fn rec(id: u64, duration: f64, synced: bool) -> TrackLyrics {
        TrackLyrics {
            id,
            timing_mismatch: false,
            relevance: id as usize,
            track_name: "Echo".into(),
            artist_name: "Crusher-P".into(),
            album_name: String::new(),
            duration: Some(duration),
            kind: if synced {
                LyricsKind::Synced(vec![LyricLine {
                    at: 0.0,
                    text: "x".into(),
                }])
            } else {
                LyricsKind::Plain(vec!["x".into()])
            },
        }
    }

    fn ids(v: &[TrackLyrics]) -> Vec<u64> {
        v.iter().map(|c| c.id).collect()
    }

    /// A raw lrclib record, as a rung's response would carry it.
    fn raw(id: u64) -> Lyrics {
        Lyrics {
            id,
            name: "Echo".into(),
            track_name: "Echo".into(),
            artist_name: "Crusher-P".into(),
            album_name: String::new(),
            duration: Some(245.0),
            instrumental: false,
            plain_lyrics: Some("x".into()),
            synced_lyrics: None,
        }
    }

    // ── query normalisation ───────────────────────────────────────────────

    #[test]
    fn strips_production_decoration_from_titles() {
        assert_eq!(strip_title_noise("法螺話 (Official Video)"), "法螺話");
        assert_eq!(strip_title_noise("法螺話 [MV]"), "法螺話");
        assert_eq!(strip_title_noise("法螺話 (feat. Guiano)"), "法螺話");
        assert_eq!(strip_title_noise("Ribs (Official Music Video)"), "Ribs");
        assert_eq!(strip_title_noise("Song [Lyrics] (HD)"), "Song");
        assert_eq!(strip_title_noise("Song【MV】"), "Song");
        assert_eq!(strip_title_noise("Song (Remastered)"), "Song");
    }

    #[test]
    fn keeps_qualifiers_that_denote_a_different_recording() {
        // These change which recording it is, so stripping them would match
        // the wrong lyrics.
        assert_eq!(
            strip_title_noise("法螺話(self cover)"),
            "法螺話(self cover)"
        );
        assert_eq!(strip_title_noise("Song (Remix)"), "Song (Remix)");
        assert_eq!(strip_title_noise("Song (Acoustic)"), "Song (Acoustic)");
        assert_eq!(
            strip_title_noise("Song (Live at Budokan)"),
            "Song (Live at Budokan)"
        );
    }

    #[test]
    fn title_stripping_is_safe_on_odd_input() {
        assert_eq!(strip_title_noise(""), "");
        assert_eq!(strip_title_noise("Plain Title"), "Plain Title");
        // Unbalanced brackets must not panic or eat the rest of the string.
        assert_eq!(strip_title_noise("Song (Official"), "Song (Official");
        assert_eq!(strip_title_noise("Song )stray("), "Song )stray(");
        // Whitespace left by a removed group is collapsed.
        assert_eq!(strip_title_noise("A  (official video)  B"), "A B");
    }

    #[test]
    fn reduces_cover_uploads_to_the_original_song_name() {
        assert_eq!(
            song_title_only("【歌ってみた】人マニア / covered by ヰ世界情緒", ""),
            "人マニア"
        );
        assert_eq!(
            song_title_only("人マニア / covered by ヰ世界情緒", ""),
            "人マニア"
        );
        assert_eq!(song_title_only("Song (Cover)", ""), "Song");
        assert_eq!(song_title_only("Song 【cover】", ""), "Song");
        assert_eq!(song_title_only("Song - covered by Someone", ""), "Song");
        assert_eq!(song_title_only("「歌ってみた」Song", ""), "Song");
        // `Song / Artist` is a common Japanese upload convention.
        assert_eq!(song_title_only("人マニア / ヰ世界情緒", ""), "人マニア");
    }

    #[test]
    fn strips_japanese_version_credits() {
        assert_eq!(song_title_only("ダーリン ver.わかばやし", ""), "ダーリン");
        assert_eq!(song_title_only("ダーリンver.わかばやし", ""), "ダーリン");
        assert_eq!(song_title_only("Song ver.Someone", ""), "Song");
        assert_eq!(song_title_only("Song ver: Someone", ""), "Song");
    }

    #[test]
    fn version_marker_does_not_fire_mid_word() {
        // "Cover." contains "ver." — cutting there would leave "Co".
        assert_eq!(song_title_only("Cover.jp Anthem", ""), "Cover.jp Anthem");
        assert_eq!(song_title_only("Discover.me", ""), "Discover.me");
    }

    #[test]
    fn strips_the_english_alias_youtube_music_appends() {
        // Real titles from the app log — lrclib stores only the original.
        assert_eq!(song_title_only("法螺話 - Tall Story", ""), "法螺話");
        assert_eq!(
            song_title_only("キャラクターT - Character T (feat. Kasane Teto)", ""),
            "キャラクターT"
        );
        assert_eq!(song_title_only("泡沫 - Utakata", ""), "泡沫");
        assert_eq!(song_title_only("千鳥 - Plover", ""), "千鳥");
        assert_eq!(
            song_title_only("食虫植物 - Carnivorous Plant", ""),
            "食虫植物"
        );
        assert_eq!(
            song_title_only("ハナタバ - Hanataba (feat. KAFU)", ""),
            "ハナタバ"
        );
        assert_eq!(
            song_title_only("マインドブランド - Mind brand", ""),
            "マインドブランド"
        );
        // Punctuation inside the original title is part of it.
        assert_eq!(
            song_title_only("フィクションです。 - It’s Fiction.", ""),
            "フィクションです。"
        );
        // A parenthesised alias is kept — only the trailing credit is cut.
        assert_eq!(
            song_title_only("逆さ月 (Reverse Moon) feat. asmi", ""),
            "逆さ月 (Reverse Moon)"
        );
    }

    #[test]
    fn hyphen_split_needs_surrounding_spaces() {
        // A hyphenated word is not a field separator.
        assert_eq!(song_title_only("Twenty-One", ""), "Twenty-One");
        assert_eq!(song_title_only("Re-Education", ""), "Re-Education");
    }

    #[test]
    fn strips_a_bare_by_credit_only_on_covers() {
        // Real title from the app log.
        assert_eq!(
            song_title_only("【歌ってみた】ドゥーマー by 花譜", ""),
            "ドゥーマー"
        );
        assert_eq!(song_title_only("Song (cover) by Someone", ""), "Song");

        // Without a cover marker, `by` is part of the name.
        assert_eq!(song_title_only("Stand By Me", ""), "Stand By Me");
        assert_eq!(song_title_only("Get By", ""), "Get By");
        assert_eq!(song_title_only("Drive By", ""), "Drive By");
    }

    #[test]
    fn cover_marker_detection_ignores_substrings() {
        // "cover" inside another word must not make this look like a cover,
        // which would then license the `by` cut.
        assert!(!has_cover_marker("Undiscovered"));
        assert!(!has_cover_marker("Recovery"));
        assert!(has_cover_marker("Song (Cover)"));
        assert!(has_cover_marker("【歌ってみた】Song"));
        assert!(has_cover_marker("Song ver.X"));
        // A guest credit is not a cover, so it must not license the `by` cut.
        assert!(!has_cover_marker("Stand By Me feat. Someone"));
        assert_eq!(
            song_title_only("Stand By Me feat. Someone", ""),
            "Stand By Me"
        );
        assert_eq!(
            song_title_only("Discovery by Daft Punk", ""),
            "Discovery by Daft Punk"
        );
    }

    #[test]
    fn song_title_only_leaves_ordinary_titles_alone() {
        assert_eq!(song_title_only("Ribs", ""), "Ribs");
        assert_eq!(song_title_only("法螺話", ""), "法螺話");
        assert_eq!(song_title_only("", ""), "");
        // A kept multi-byte bracket group must not slice mid-character:
        // `】` is three bytes, so an inclusive range ends inside it.
        assert_eq!(
            strip_title_noise("【あいうえお】Song"),
            "【あいうえお】Song"
        );
        assert_eq!(song_title_only("「そら」Song", ""), "「そら」Song");
        // No panic on unbalanced or odd input.
        assert_eq!(song_title_only("Song (cover", ""), "Song (cover");
        song_title_only("【】", "");
        song_title_only("///", "");
        song_title_only("【】【】(((", "");
    }

    #[test]
    fn cover_reduction_is_stricter_than_noise_stripping() {
        // strip_title_noise is used by the precise steps and must preserve a
        // qualifier that denotes a different recording; song_title_only is the
        // broad fallback and may discard it.
        let t = "法螺話(self cover)";
        assert_eq!(strip_title_noise(t), t);
        assert_eq!(song_title_only(t, ""), "法螺話");
    }

    #[test]
    fn normalises_youtube_artist_forms() {
        // YouTube's auto-generated channels are named "<artist> - Topic".
        assert_eq!(primary_artist("理芽 - Topic"), "理芽");
        // lrclib stores one artist per record, so a joined list matches nothing.
        assert_eq!(primary_artist("理芽, Guiano"), "理芽");
        // Japanese credits join with the ideographic comma.
        assert_eq!(primary_artist("重音テト、音街ウナ"), "重音テト");
        assert_eq!(primary_artist("Lorde"), "Lorde");
        assert_eq!(primary_artist(""), "");
        // `&` is part of plenty of band names, so it is not a separator.
        assert_eq!(primary_artist("Simon & Garfunkel"), "Simon & Garfunkel");
    }

    #[test]
    fn artist_leading_titles_keep_the_song_not_the_artist() {
        // Uploads outside YouTube's Topic channels are titled `Artist - Song`,
        // where keeping the leading field would search lrclib for the artist.
        assert_eq!(song_title_only("Lorde - Ribs", "Lorde"), "Ribs");
        assert_eq!(song_title_only("理芽 - 法螺話", "理芽 - Topic"), "法螺話");
        assert_eq!(
            song_title_only("Syn Cole - Sway", "Syn Cole, Nevve"),
            "Sway"
        );
        // Decoration on such a title is still dropped.
        assert_eq!(
            song_title_only("Lorde - Ribs (Official Video)", "Lorde"),
            "Ribs"
        );

        // The leading field only yields when it really is the artist — the
        // alias layout must keep behaving as it did.
        assert_eq!(song_title_only("法螺話 - Tall Story", "理芽"), "法螺話");
        assert_eq!(song_title_only("Ribs - Live", "Lorde"), "Ribs");
        // And with no artist to compare against, nothing changes.
        assert_eq!(song_title_only("Lorde - Ribs", ""), "Lorde");
    }

    #[test]
    fn ladder_broadens_and_collapses_duplicates() {
        // Clean metadata with no album: the first two steps are identical, so
        // a well-tagged track must not pay for a duplicate request.
        let clean = LyricsQuery {
            title: "Ribs".into(),
            artist: "Lorde".into(),
            album: String::new(),
            duration: Some(221.0),
        };
        let ladder = clean.search_ladder();
        let mut uniq: Vec<String> = ladder.iter().map(|a| format!("{a:?}")).collect();
        let before = uniq.len();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            before,
            uniq.len(),
            "duplicate rungs cost extra requests: {ladder:?}"
        );
        assert_eq!(
            ladder[0],
            Attempt::Meta {
                track: "Ribs".into(),
                artist: "Lorde".into(),
                album: String::new()
            }
        );

        // Messy metadata: the album is dropped, then the title cleaned, then
        // free text — every step actually differs.
        let messy = LyricsQuery {
            title: "法螺話 (Official Video)".into(),
            artist: "理芽 - Topic".into(),
            album: "幻朧".into(),
            duration: Some(198.0),
        };
        let ladder = messy.search_ladder();
        assert!(matches!(&ladder[1], Attempt::Meta { album, .. } if album.is_empty()));
        assert!(
            ladder
                .iter()
                .any(|a| matches!(a, Attempt::Meta { track, artist, .. }
                if track == "法螺話" && artist == "理芽")),
            "ladder never tries the normalised form: {ladder:?}"
        );
        assert!(
            ladder
                .iter()
                .any(|a| matches!(a, Attempt::FreeText(q) if q == "法螺話 理芽"))
        );
    }

    /// A record carrying the given timed lines.
    fn timed(id: u64, duration: f64, lines: &[(f64, &str)]) -> TrackLyrics {
        TrackLyrics {
            kind: LyricsKind::Synced(
                lines
                    .iter()
                    .map(|(at, text)| LyricLine {
                        at: *at,
                        text: (*text).into(),
                    })
                    .collect(),
            ),
            ..rec(id, duration, true)
        }
    }

    #[test]
    fn duplicate_lyrics_collapse_to_the_best_ranked_copy() {
        // The common lrclib shape: one file re-uploaded under three albums.
        let lines = &[(0.0, "a"), (1.5, "b")][..];
        let mut first = timed(1, 245.0, lines);
        first.album_name = "Single".into();
        let mut second = timed(2, 245.0, lines);
        second.album_name = "Deluxe".into();
        let mut third = timed(3, 245.0, lines);
        third.album_name = "Greatest Hits".into();

        let out = dedupe_by_content(rank(vec![first, second, third], &q(Some(245.0))), None);
        assert_eq!(ids(&out), [1], "the best-ranked copy is the one kept");
    }

    #[test]
    fn same_words_with_different_timings_are_both_kept() {
        // The whole point of the picker: these are a real choice, and which one
        // scrolls correctly is exactly what the user is choosing between.
        let a = timed(1, 245.0, &[(0.0, "a"), (1.5, "b")]);
        let b = timed(2, 245.0, &[(0.0, "a"), (2.5, "b")]);
        let out = dedupe_by_content(rank(vec![a, b], &q(Some(245.0))), None);
        assert_eq!(ids(&out), [1, 2]);
    }

    #[test]
    fn dedupe_distinguishes_kinds_and_collapses_instrumentals() {
        // Same words, but one has timings and one doesn't — different records.
        let synced = timed(1, 245.0, &[(0.0, "a")]);
        let plain = TrackLyrics {
            kind: LyricsKind::Plain(vec!["a".into()]),
            ..rec(2, 245.0, false)
        };
        let out = dedupe_by_content(vec![synced, plain], None);
        assert_eq!(ids(&out), [1, 2]);

        // Instrumentals carry nothing to tell apart, so one row is enough.
        let inst = |id| TrackLyrics {
            kind: LyricsKind::Instrumental,
            ..rec(id, 245.0, false)
        };
        assert_eq!(ids(&dedupe_by_content(vec![inst(1), inst(2)], None)), [1]);
    }

    #[test]
    fn the_record_on_screen_survives_deduplication() {
        let lines = &[(0.0, "a"), (1.5, "b")][..];
        let copies = || {
            vec![
                timed(1, 245.0, lines),
                timed(2, 245.0, lines),
                timed(3, 245.0, lines),
            ]
        };

        // #3 is what the panel is showing. #1 ranks better, but collapsing to
        // it would leave the picker unable to say which row is in use.
        assert_eq!(ids(&dedupe_by_content(copies(), Some(3))), [3]);
        // Exactly one row still represents the group.
        assert_eq!(ids(&dedupe_by_content(copies(), Some(1))), [1]);
        // A record that isn't in the list changes nothing.
        assert_eq!(ids(&dedupe_by_content(copies(), Some(99))), [1]);

        // Protecting one group leaves the others alone.
        let mixed = vec![
            timed(1, 245.0, lines),
            timed(4, 245.0, &[(0.0, "z")]),
            timed(3, 245.0, lines),
        ];
        assert_eq!(ids(&dedupe_by_content(mixed, Some(3))), [4, 3]);
    }

    #[test]
    fn dedupe_ignores_metadata_and_leaves_order_alone() {
        // Album, artist spelling and lrclib id differ; the lyrics don't.
        let mut a = timed(1, 245.0, &[(0.0, "x")]);
        a.artist_name = "Crusher-P".into();
        let mut b = timed(2, 246.0, &[(0.0, "x")]);
        b.artist_name = "crusher p".into();
        b.album_name = "Other".into();
        let distinct = timed(3, 245.0, &[(0.0, "y")]);

        let out = dedupe_by_content(vec![a, distinct, b], None);
        assert_eq!(ids(&out), [1, 3], "order of survivors is untouched");
    }

    #[test]
    fn relevance_never_restarts_across_rungs() {
        let mut pool = Vec::new();
        let mut seen = HashSet::new();
        // 0 is reserved for the `/get` hit `best_for` folds in, so no search
        // result may claim it.
        let mut next = 1;

        next = merge_rung(&mut pool, &mut seen, vec![raw(1), raw(2), raw(3)], next);
        assert_eq!(next, 4);

        // A broader rung repeats what the first found and adds one record. The
        // repeats are dropped, but they still consumed their slots.
        next = merge_rung(
            &mut pool,
            &mut seen,
            vec![raw(1), raw(2), raw(3), raw(4)],
            next,
        );
        assert_eq!(next, 8);

        // Counting only the records *kept* would restart this rung at 4 and
        // stamp #5 ahead of #4 — a broader query outranking a narrower one.
        next = merge_rung(&mut pool, &mut seen, vec![raw(5)], next);
        assert_eq!(next, 9);

        assert_eq!(
            pool.iter().map(|c| (c.id, c.relevance)).collect::<Vec<_>>(),
            [(1, 1), (2, 2), (3, 3), (4, 7), (5, 8)]
        );
        assert!(
            pool.windows(2).all(|w| w[0].relevance < w[1].relevance),
            "a later rung was stamped ahead of an earlier one"
        );
        assert!(pool.iter().all(|c| c.relevance > 0), "0 is the /get hit's");
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_is_retried() {
        let calls = std::cell::Cell::new(0);
        let out = with_retry("test", || {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                match n {
                    1 => Err(LrcError::Api {
                        message: String::new(),
                        name: String::new(),
                        status_code: 503,
                    }),
                    _ => Ok(n),
                }
            }
        })
        .await;

        assert_eq!(out.expect("the retry should have succeeded"), 2);
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_settled_answer_is_not_retried() {
        // A 404 is lrclib saying it holds no such record. Asking again wastes
        // the user's time and lrclib's bandwidth.
        let calls = std::cell::Cell::new(0);
        let out: std::result::Result<(), LrcError> = with_retry("test", || {
            calls.set(calls.get() + 1);
            async {
                Err(LrcError::Api {
                    message: String::new(),
                    name: String::new(),
                    status_code: 404,
                })
            }
        })
        .await;

        assert!(out.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_are_bounded() {
        let calls = std::cell::Cell::new(0);
        let out: std::result::Result<(), LrcError> = with_retry("test", || {
            calls.set(calls.get() + 1);
            async {
                Err(LrcError::Api {
                    message: String::new(),
                    name: String::new(),
                    status_code: 500,
                })
            }
        })
        .await;

        assert!(out.is_err());
        assert_eq!(
            calls.get(),
            MAX_RETRIES as i32 + 1,
            "a server that stays down must not be hammered"
        );
    }

    /// A synced record with `n` distinct lines, so copies don't collide.
    fn with_lines(id: u64, duration: f64, n: usize) -> TrackLyrics {
        let lines: Vec<(f64, String)> = (0..n).map(|i| (i as f64, format!("line {i}"))).collect();
        timed(
            id,
            duration,
            &lines
                .iter()
                .map(|(at, t)| (*at, t.as_str()))
                .collect::<Vec<_>>(),
        )
    }

    fn by(id: u64, duration: f64, artist: &str) -> TrackLyrics {
        TrackLyrics {
            artist_name: artist.into(),
            ..rec(id, duration, true)
        }
    }

    #[test]
    fn the_right_artist_beats_the_closer_duration() {
        // Both reported: a different "Ride It" matched the length exactly and
        // won, and "Do Better" matched a K-pop group over Feint. A record for
        // another artist's song is not a better match for being the right
        // number of seconds long.
        let out = rank(
            vec![by(1, 245.0, "Somebody Else"), by(2, 246.0, "Crusher-P")],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
    }

    #[test]
    fn artist_matching_does_not_fire_on_a_substring() {
        assert!(credits_artist("crusher-p", "crusher-p"));
        assert!(credits_artist(
            "larissa lambert, jay sean",
            "larissa lambert"
        ));
        assert!(credits_artist("feint & laura brehm", "feint"));
        // The reason this is boundary-checked at all: promoted to a primary
        // sort key, an accidental match would decide the ranking.
        assert!(!credits_artist("adore", "ado"));
        assert!(!credits_artist("shadow", "ado"));
        assert!(!credits_artist(
            "shaneil muir, vybz kartel",
            "larissa lambert"
        ));
        // Japanese credits run names straight into punctuation and each other,
        // so only ASCII counts as continuing a word.
        assert!(credits_artist("理芽, guiano", "理芽"));
        assert!(credits_artist("重音テト、音街ウナ", "重音テト"));
        // Nothing to match on means no record can claim the artist.
        assert!(!credits_artist("anyone", ""));
    }

    #[test]
    fn a_fragment_loses_to_the_whole_song() {
        // The reported アイドル case: a 33-line record whose timings start 22s
        // in, picked over a complete 78-line one because its length matched.
        let out = rank(
            vec![
                with_lines(1, 245.0, 10),
                with_lines(2, 246.0, 40),
                with_lines(3, 247.0, 38),
                with_lines(4, 248.0, 42),
            ],
            &q(Some(245.0)),
        );
        assert_eq!(
            out[0].id,
            2,
            "a complete record should lead: {:?}",
            ids(&out)
        );
        assert_eq!(out.last().unwrap().id, 1, "the fragment sorts last");
    }

    #[test]
    fn a_fragment_uploaded_many_times_cannot_set_the_standard() {
        // Two thirds of lrclib's results for a popular track are copies. If
        // every copy counted, a stub re-uploaded five times would look typical
        // and the complete records would be the outliers.
        let mut items: Vec<TrackLyrics> = (1..=5).map(|i| with_lines(i, 245.0, 10)).collect();
        items.push(with_lines(6, 245.0, 40));
        items.push(with_lines(7, 245.0, 41));
        assert_eq!(typical_line_count(&items, Some(245.0)), Some(40));

        // Counting every copy would have made the median 10.
        assert!(is_stub(
            &items[0],
            Some(245.0),
            typical_line_count(&items, Some(245.0))
        ));
        assert!(!is_stub(
            &items[5],
            Some(245.0),
            typical_line_count(&items, Some(245.0))
        ));
    }

    #[test]
    fn a_different_song_of_the_same_name_is_no_yardstick() {
        // The reported `GURU` cover: the pool held several unrelated songs of
        // that name, whose line counts pushed the median to 54 and made the
        // correct 24-line record look like a fragment. Only records close
        // enough in length to be this recording set the standard.
        let items = vec![
            with_lines(1, 197.0, 24), // this recording
            with_lines(2, 212.0, 54), // another song that shares the title
            with_lines(3, 215.0, 60),
            with_lines(4, 220.0, 68),
        ];
        let typical = typical_line_count(&items, Some(197.0));
        assert_eq!(typical, None, "none of them are this recording");
        assert!(!is_stub(&items[0], Some(197.0), typical));

        // Ranked, the short record still wins on length.
        let out = rank(items, &q(Some(197.0)));
        assert_eq!(out[0].id, 1);
    }

    #[test]
    fn too_few_candidates_to_call_anything_a_fragment() {
        // With one or two records there is no spread to judge against, and a
        // short song would be demoted for being short.
        let items = vec![with_lines(1, 245.0, 4), with_lines(2, 245.0, 40)];
        assert_eq!(typical_line_count(&items, Some(245.0)), None);
        assert!(!is_stub(&items[0], Some(245.0), None));
    }

    #[test]
    fn an_instrumental_is_not_a_fragment() {
        // It has no words to be missing.
        let inst = TrackLyrics {
            kind: LyricsKind::Instrumental,
            ..rec(9, 245.0, false)
        };
        assert!(!is_stub(&inst, Some(245.0), Some(40)));
    }

    #[test]
    fn synced_outranks_plain() {
        let out = rank(
            vec![rec(1, 245.0, false), rec(2, 245.0, true)],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
    }

    #[test]
    fn closer_duration_wins_among_equals() {
        let out = rank(
            vec![
                rec(1, 250.0, true),
                rec(2, 245.0, true),
                rec(3, 248.0, true),
            ],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 3, 1]);
    }

    /// End-to-end against the real API: fetch, parse, rank and select. Ignored
    /// by default so `cargo test` stays offline.
    /// Run with `cargo test -p ytm-core -- --ignored`.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn best_for_returns_synced_lyrics_end_to_end() {
        let svc = LyricsService::new();
        let query = LyricsQuery {
            title: "Bohemian Rhapsody".into(),
            artist: "Queen".into(),
            album: String::new(),
            duration: Some(354.0),
        };

        let found = svc
            .best_for(&query, None)
            .await
            .expect("lookup failed")
            .expect("no lyrics found");

        assert!(
            found.is_synced(),
            "expected synced lyrics, got {:?}",
            found.kind
        );
        let lines = found.synced_lines().expect("synced");
        assert!(lines.len() > 10, "suspiciously few lines: {}", lines.len());
        // Timestamps must be sorted and land inside the track.
        assert!(lines.windows(2).all(|w| w[0].at <= w[1].at));
        assert!(lines.last().unwrap().at < 400.0);
        // The highlight lookup must actually move over the track's span.
        assert_eq!(active_index(lines, -1.0), None);
        assert!(active_index(lines, 200.0).is_some());
    }

    /// Regression for the reported failure: lrclib has this track (id 28584145)
    /// but every YouTube-flavoured spelling of its metadata used to return
    /// "no lyrics found".
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_despite_youtube_metadata_noise() {
        let svc = LyricsService::new();
        for (title, artist, album) in [
            ("法螺話 (feat. Guiano)", "理芽", "幻朧"),
            ("法螺話 (Official Video)", "理芽", ""),
            ("法螺話 [MV]", "理芽", ""),
            ("法螺話", "理芽 - Topic", ""),
            ("法螺話", "理芽, Guiano", "幻朧"),
        ] {
            let q = LyricsQuery {
                title: title.into(),
                artist: artist.into(),
                album: album.into(),
                duration: Some(198.0),
            };
            let found = svc
                .best_for(&q, None)
                .await
                .unwrap_or_else(|e| panic!("{title:?} / {artist:?} errored: {e}"));
            let found =
                found.unwrap_or_else(|| panic!("no lyrics for {title:?} / {artist:?} / {album:?}"));
            assert!(
                found.is_synced(),
                "{title:?} matched #{} but unsynced",
                found.id
            );
        }
    }

    /// Cover uploads: lrclib holds the original, not the cover, so the lookup
    /// only succeeds if the title is reduced to the song name and the artist
    /// constraint is dropped.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_for_a_cover_upload() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "【歌ってみた】人マニア / covered by ヰ世界情緒".into(),
            artist: "ヰ世界情緒".into(),
            album: String::new(),
            duration: Some(128.0),
        };
        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found for the cover");
        assert!(found.is_synced(), "expected synced, got {:?}", found.kind);
        assert!(
            found.track_name.contains("人マニア"),
            "matched the wrong song: {}",
            found.track_name
        );
        // The picker must offer the alternatives too.
        assert!(
            svc.candidates(&q, None)
                .await
                .expect("search errored")
                .len()
                > 1
        );
    }

    /// A single malformed record (lrclib returns `duration: null` for some)
    /// used to fail the whole response, collapsing the result set to whatever a
    /// broader fallback query returned.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn one_bad_record_does_not_discard_the_rest() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "Echo".into(),
            artist: "Crusher-P".into(),
            album: String::new(),
            duration: Some(244.0),
        };
        // Assert on the raw response: one record in it has `duration: null`,
        // which used to fail the whole array. The ranked list is now
        // duration-filtered, so its length is not the right signal.
        let raw = svc
            .client
            .search_by_meta("Echo", "Crusher-P", "")
            .await
            .expect("search errored");
        assert!(
            raw.len() > 15,
            "a malformed record discarded the rest: only {} survived",
            raw.len()
        );
        assert!(
            raw.iter().any(|l| l.duration.is_none()),
            "no null-duration record in this response — the regression is untested"
        );

        // And the best match must be the closest-timed one available.
        let best = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics");
        let delta = best.duration_delta(q.duration).expect("no duration");
        assert!(
            delta <= DECISIVE_DURATION_DELTA,
            "picked #{} at {:?}s — {delta}s off a 244s track",
            best.id,
            best.duration
        );
    }

    /// `ダーリン ver.わかばやし` — a rendition credit with no brackets at all.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_for_a_version_credit() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "ダーリン ver.わかばやし".into(),
            artist: "わかばやし".into(),
            album: String::new(),
            duration: Some(275.0),
        };
        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found");
        assert!(
            found.track_name.contains("ダーリン"),
            "matched the wrong song: {}",
            found.track_name
        );
    }

    /// The picker must offer everything the ladder can reach, not just the
    /// first rung that happened to match — a precise query returning one record
    /// used to hide the dozen a broader one would have found.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn picker_aggregates_across_the_whole_ladder() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "Echo".into(),
            artist: "Crusher-P".into(),
            album: String::new(),
            duration: Some(244.0),
        };

        let first = svc
            .search_ladder(&q, true, None)
            .await
            .expect("search errored");
        let all = svc.candidates(&q, None).await.expect("search errored");
        assert!(
            all.len() > first.len(),
            "picker showed {} but the ladder reaches {}",
            all.len(),
            first.len()
        );

        // Merged, so no record may appear twice.
        let mut ids: Vec<u64> = all.iter().map(|c| c.id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate records in the picker");
    }

    /// `キャラクターT` — reported as finding nothing.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_for_character_t() {
        let svc = LyricsService::new();
        // Even with an artist and album that don't match lrclib at all.
        let q = LyricsQuery {
            title: "キャラクターT".into(),
            artist: "atena - Topic".into(),
            album: "Some Album".into(),
            duration: Some(174.0),
        };
        // lrclib has three records: two at 174s and one at 181s. All three are
        // offered — 7s is the same song — but the 181s one loses its timings.
        let all = svc.candidates(&q, None).await.expect("search errored");
        let summary: Vec<_> = all
            .iter()
            .map(|c| (c.id, c.duration, c.is_synced(), c.timing_mismatch))
            .collect();
        assert_eq!(all.len(), 3, "got {summary:?}");
        for c in &all {
            let delta = c.duration_delta(q.duration).expect("no duration");
            assert_eq!(
                c.timing_mismatch,
                delta > SYNC_DURATION_DELTA,
                "#{} at {delta}s off: {summary:?}",
                c.id
            );
            // A demoted record must never still claim to be synced.
            assert!(!(c.timing_mismatch && c.is_synced()));
        }

        let best = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found");
        assert!(best.is_synced());
        assert_eq!(best.duration_delta(q.duration), Some(0.0));
    }

    /// The exact titles the app logged as finding nothing. YouTube Music
    /// appends an English alias (`法螺話 - Tall Story`) that lrclib doesn't have.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_for_youtube_alias_titles() {
        let svc = LyricsService::new();
        for (title, artist, album, dur) in [
            ("法螺話 - Tall Story", "理芽", "", 198.0),
            (
                "キャラクターT - Character T (feat. Kasane Teto)",
                "Atena",
                "",
                174.0,
            ),
            ("食虫植物 - Carnivorous Plant", "理芽", "", 158.0),
            ("マインドブランド - Mind brand", "MARETU", "", 261.0),
        ] {
            let q = LyricsQuery {
                title: title.into(),
                artist: artist.into(),
                album: album.into(),
                duration: Some(dur),
            };
            let found = svc
                .best_for(&q, None)
                .await
                .unwrap_or_else(|e| panic!("{title:?} errored: {e}"))
                .unwrap_or_else(|| panic!("no lyrics for {title:?}"));
            assert!(
                found.is_synced(),
                "{title:?} matched unsynced #{}",
                found.id
            );
            assert!(
                svc.candidates(&q, None)
                    .await
                    .expect("search errored")
                    .len()
                    > 1,
                "{title:?} offered only one choice"
            );
        }
    }

    /// `Approve Please, Genie!` has fifteen records, fourteen unsynced and one
    /// synced under a blank album. The precise rungs return only unsynced ones,
    /// so stopping at the first rung with *any* result never reached it.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn prefers_synced_even_when_it_needs_a_broader_query() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "Approve Please, Genie!".into(),
            artist: "TRAP CHICK, 重音テト, 音街ウナ".into(),
            album: "Approve Please, Genie!".into(),
            // 2:44 against a 2:42 transcription — inside the window.
            duration: Some(164.0),
        };
        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found");
        assert!(
            found.is_synced(),
            "picked unsynced #{} at {:?}s when a synced 162s record exists",
            found.id,
            found.duration
        );
    }

    /// `【歌ってみた】ドゥーマー by 花譜` — a cover whose credit is a bare
    /// `by <name>` with no bracket or separator to key off.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn finds_lyrics_for_a_bare_by_credit() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "【歌ってみた】ドゥーマー by 花譜".into(),
            artist: "花譜".into(),
            album: String::new(),
            duration: Some(157.0),
        };
        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found");
        assert!(found.is_synced(), "matched unsynced #{}", found.id);
        assert!(
            found.duration_delta(q.duration).is_some_and(|d| d <= 2.0),
            "picked #{} at {:?}s for a 157s track",
            found.id,
            found.duration
        );
    }

    /// lrclib returns hits in its own relevance order — exact title matches
    /// ahead of decorated variants — and we carry that through as the final
    /// tiebreak. This checks the assumption still holds upstream.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn lrclib_results_are_relevance_ordered() {
        let svc = LyricsService::new();
        let raw = svc
            .client
            .search_by_meta("法螺話", "", "")
            .await
            .expect("search errored");
        assert!(raw.len() > 3);

        // Exact title matches must precede decorated ones.
        let last_exact = raw.iter().rposition(|l| l.track_name == "法螺話");
        let first_decorated = raw.iter().position(|l| l.track_name != "法螺話");
        if let (Some(last), Some(first)) = (last_exact, first_decorated) {
            assert!(
                last < first,
                "lrclib no longer returns relevance-ordered results: {:?}",
                raw.iter().map(|l| &l.track_name).collect::<Vec<_>>()
            );
        }

        // And the order is stable, so using it as a tiebreak is deterministic.
        let again = svc
            .client
            .search_by_meta("法螺話", "", "")
            .await
            .expect("search errored");
        assert_eq!(
            raw.iter().map(|l| l.id).collect::<Vec<_>>(),
            again.iter().map(|l| l.id).collect::<Vec<_>>()
        );
    }

    /// `Sway (feat. Nevve)` at 3:21. lrclib's exact 201s record is titled just
    /// `Sway`, while `/get` and the precise rungs both return a 202s one — so
    /// accepting either without broadening picked the 3:22.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn picks_the_exact_duration_over_a_near_miss() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "Sway (feat. Nevve)".into(),
            artist: "Syn Cole, Nevve".into(),
            album: String::new(),
            duration: Some(201.0),
        };
        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics found");
        assert!(found.is_synced());
        assert_eq!(
            found.duration,
            Some(201.0),
            "picked #{} at {:?}s over the exact 201s record",
            found.id,
            found.duration
        );
    }

    /// `【歌ってみた】ヴァンパイア/ covered by ヰ世界情緒` runs 3:13 while every
    /// lrclib record of the original is 3:00–3:02. Thirteen seconds is far too
    /// much drift to scroll, but the words are the same song, so the lyrics are
    /// offered as plain text instead of the panel reporting nothing.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn offers_plain_lyrics_when_only_the_timing_is_wrong() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "【歌ってみた】ヴァンパイア/ covered by ヰ世界情緒".into(),
            artist: "ヰ世界情緒".into(),
            album: String::new(),
            duration: Some(193.0),
        };

        let found = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics offered for a 13s difference");
        assert!(
            matches!(found.kind, LyricsKind::Plain(_)),
            "expected plain, got {:?}",
            found.kind
        );
        assert!(found.timing_mismatch);

        // The picker offers alternatives, and none of them claims timings.
        let all = svc.candidates(&q, None).await.expect("search errored");
        assert!(all.len() > 1, "only {} candidate(s)", all.len());
        assert!(
            all.iter().all(|c| !c.is_synced()),
            "a record 13s out was offered as synced"
        );
        // The wildly-off records (1:38, 4:22) are a different song, not a
        // different edit, and must not be offered at all.
        assert!(
            all.iter().all(|c| c
                .duration_delta(q.duration)
                .is_none_or(|d| d <= MAX_DURATION_DELTA)),
            "a different song leaked into the results"
        );
    }

    /// The picker must always be able to show what is already in use — both
    /// the automatic match, which is frequently the `/get` hit the ladder never
    /// sees, and a previous manual choice, which resolves the same way.
    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn the_picker_always_lists_the_record_in_use() {
        let svc = LyricsService::new();
        let q = LyricsQuery {
            title: "Bohemian Rhapsody".into(),
            artist: "Queen".into(),
            album: String::new(),
            duration: Some(354.0),
        };

        let best = svc
            .best_for(&q, None)
            .await
            .expect("lookup errored")
            .expect("no lyrics");

        let all = svc
            .candidates(&q, Some(best.id))
            .await
            .expect("search errored");
        assert!(
            all.iter().any(|c| c.id == best.id),
            "the automatic match #{} is missing from its own picker",
            best.id
        );
        // And it appears exactly once, not alongside its own duplicates.
        assert_eq!(all.iter().filter(|c| c.id == best.id).count(), 1);
    }

    #[tokio::test]
    #[ignore = "hits the live lrclib.net API"]
    async fn candidates_returns_multiple_ranked_records() {
        let svc = LyricsService::new();
        let query = LyricsQuery {
            title: "Bohemian Rhapsody".into(),
            artist: "Queen".into(),
            album: String::new(),
            duration: Some(354.0),
        };

        let found = svc.candidates(&query, None).await.expect("search failed");
        assert!(
            found.len() > 1,
            "picker needs several options, got {}",
            found.len()
        );
        // rank() must float a synced record to the top.
        assert!(found[0].is_synced());
    }

    #[test]
    fn synced_still_beats_a_closer_plain_match() {
        // Duration is only a tiebreak — a perfectly-matching plain record must
        // not displace a synced one, since synced is the whole point.
        let out = rank(
            vec![rec(1, 245.0, false), rec(2, 249.0, true)],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
    }

    #[test]
    fn closest_duration_wins_even_by_one_second() {
        // The reported Echo case: a 4:05 (245s) record must beat a looser one
        // for a 4:04 (244s) track — and the exact 244s beats them both.
        let out = rank(
            vec![
                rec(1, 248.0, true),
                rec(2, 245.0, true),
                rec(3, 244.0, true),
            ],
            &q(Some(244.0)),
        );
        assert_eq!(ids(&out), [3, 2, 1]);
    }

    #[test]
    fn records_without_a_duration_lose_to_timed_ones() {
        // A missing duration used to be treated as a *perfect* match, because
        // the delta defaulted to 0 — so an untimed record outranked everything.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        let out = rank(vec![unknown, rec(1, 246.0, true)], &q(Some(244.0)));
        assert_eq!(ids(&out), [1, 9], "the timed record must lead");
        assert!(out[0].is_synced());
        assert!(
            !out[1].is_synced(),
            "an unverifiable record can't claim timings"
        );

        // With no known duration to compare against, it is kept and ordered
        // behind nothing in particular.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        assert_eq!(rank(vec![unknown, rec(1, 250.0, true)], &q(None)).len(), 2);
    }

    #[test]
    fn a_record_without_duration_cannot_be_trusted_for_timing() {
        // We can't tell whether it fits, so it can't be vouched for.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        let out = rank(vec![unknown], &q(Some(244.0)));
        assert_eq!(out.len(), 1, "it is still offered");
        assert!(
            out[0].timing_mismatch,
            "but its timings can't be vouched for"
        );
        assert!(!out[0].is_synced());

        // With no track duration to compare against, it is kept.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        assert_eq!(ids(&rank(vec![unknown], &q(None))), [9]);
    }

    #[test]
    fn far_off_durations_are_filtered() {
        let out = rank(
            vec![rec(1, 245.0, true), rec(2, 400.0, true)],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [1]);
    }

    #[test]
    fn a_mistimed_record_is_offered_as_plain_rather_than_dropped() {
        // The reported cover: 3:13 against a 3:00 original. Thirteen seconds is
        // far too much for synced playback, but the words are the same song.
        let out = rank(vec![rec(1, 180.0, true)], &q(Some(193.0)));
        assert_eq!(out.len(), 1, "the words were thrown away with the timings");
        assert!(
            !out[0].is_synced(),
            "13s of drift must not scroll as synced"
        );
        assert!(out[0].timing_mismatch);
        assert!(matches!(out[0].kind, LyricsKind::Plain(_)));
    }

    #[test]
    fn a_well_timed_record_keeps_its_timings() {
        let out = rank(vec![rec(1, 194.0, true)], &q(Some(193.0)));
        assert!(out[0].is_synced());
        assert!(!out[0].timing_mismatch);
    }

    #[test]
    fn synced_in_window_outranks_a_demoted_one() {
        // Even though the demoted record is listed first by lrclib.
        let out = rank(
            vec![rec(1, 180.0, true), rec(2, 194.0, true)],
            &q(Some(193.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
        assert!(out[0].is_synced());
        assert!(!out[1].is_synced());
    }

    #[test]
    fn demotion_does_not_apply_without_a_track_duration() {
        // Nothing to compare against, so nothing is claimed about the timings.
        let out = rank(vec![rec(1, 180.0, true)], &q(None));
        assert!(out[0].is_synced());
        assert!(!out[0].timing_mismatch);
    }

    #[test]
    fn a_different_song_is_dropped_outright() {
        // Beyond the outer bound these aren't a different edit, they're a
        // different track — not worth offering even as plain text.
        let out = rank(
            vec![rec(1, 400.0, true), rec(2, 500.0, true)],
            &q(Some(245.0)),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn the_sync_window_is_a_few_seconds_not_a_dozen() {
        // A ~15s difference used to be offered as synced; it is now demoted,
        // and the well-timed record wins.
        let out = rank(
            vec![rec(1, 259.0, true), rec(2, 248.0, true)],
            &q(Some(244.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
        assert!(out[0].is_synced(), "a 4s difference is still synced");
        assert!(!out[1].is_synced(), "a 15s difference must not scroll");
    }

    #[test]
    fn unknown_duration_skips_the_filter() {
        let out = rank(vec![rec(1, 400.0, true), rec(2, 10.0, true)], &q(None));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ties_fall_back_to_lrclib_relevance() {
        // Same length, same everything else: lrclib ranked #7 first, so it wins
        // — and it wins even when handed to us out of order, because the
        // ranking is an explicit sort key rather than an artefact of the sort
        // happening to be stable.
        let out = rank(
            vec![
                rec(9, 245.0, true),
                rec(7, 245.0, true),
                rec(8, 245.0, true),
            ],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [7, 8, 9]);
    }

    #[test]
    fn relevance_only_breaks_ties_it_cannot_override_a_better_match() {
        // lrclib ranked #1 first, but #2 is closer to the track's length.
        let out = rank(
            vec![rec(1, 249.0, true), rec(2, 245.0, true)],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 1]);

        // ...and a synced record outranks a more "relevant" unsynced one.
        let out = rank(
            vec![rec(1, 245.0, false), rec(2, 245.0, true)],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [2, 1]);
    }

    #[test]
    fn instrumental_and_empty_records_convert_correctly() {
        let base = Lyrics {
            id: 1,
            name: "n".into(),
            track_name: "t".into(),
            artist_name: "a".into(),
            album_name: String::new(),
            duration: Some(100.0),
            instrumental: true,
            plain_lyrics: None,
            synced_lyrics: None,
        };

        let inst = TrackLyrics::from_record(base.clone()).expect("instrumental is usable");
        assert_eq!(inst.kind, LyricsKind::Instrumental);

        // Not instrumental and no content at all → unusable, dropped.
        let empty = Lyrics {
            instrumental: false,
            ..base.clone()
        };
        assert!(TrackLyrics::from_record(empty).is_none());

        // Empty-string synced falls through to plain.
        let blank_synced = Lyrics {
            instrumental: false,
            synced_lyrics: Some("   ".into()),
            plain_lyrics: Some("just text".into()),
            ..base.clone()
        };
        let got = TrackLyrics::from_record(blank_synced).expect("plain is usable");
        assert_eq!(got.kind, LyricsKind::Plain(vec!["just text".into()]));

        // Unparseable synced (no timestamps) also falls through to plain.
        let bad_synced = Lyrics {
            instrumental: false,
            synced_lyrics: Some("no timestamps here".into()),
            plain_lyrics: Some("just text".into()),
            ..base
        };
        let got = TrackLyrics::from_record(bad_synced).expect("plain is usable");
        assert!(matches!(got.kind, LyricsKind::Plain(_)));
    }
}
