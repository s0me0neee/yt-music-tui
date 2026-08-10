//! Lyrics lookup against [lrclib.net](https://lrclib.net).
//!
//! [`lrclib`] is the transport; this module is the policy. No single LRCLIB
//! endpoint does what we need — `/get` takes a duration but returns one result,
//! `/search` returns many but ignores duration — so [`LyricsService::best_for`]
//! layers the two and [`rank`] does duration-proximity scoring client-side.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use lrclib::{LrcError, LrcLib, Lyrics, parse_lrc};

/// Re-exported so consumers don't need `lrclib` as a direct dependency.
pub use lrclib::{LyricLine, active_index, next_boundary};

use crate::error::Result;
use crate::library::Track;

/// Records whose length differs from the track's by more than this are hidden:
/// they are a different edit at best, and synced lyrics drift visibly out of
/// time well before this much. A few seconds of slack covers the usual
/// difference between a YouTube upload and a release master.
///
/// This is a hard limit whenever the track's own duration is known — there is
/// deliberately no "show them anyway if nothing else matched" fallback.
const MAX_DURATION_DELTA: f64 = 5.0;

/// How close the exact-lookup hit must be before it's accepted without also
/// consulting search. lrclib's `/get` tolerates a few seconds either way, and
/// the record it picks is often not the best-timed one on offer.
const EXACT_DURATION_DELTA: f64 = 2.0;

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

/// Reduces a title to just the song's name, discarding cover credits.
///
/// Cover uploads are titled like `【歌ってみた】人マニア / covered by ヰ世界情緒`.
/// LRCLIB has the *original*, so the lyrics are found by song name alone:
/// searching the full title returns nothing, and so does constraining by the
/// coverer.
///
/// Deliberately more aggressive than [`strip_title_noise`], so it is only used
/// by the late, broadening steps of the search ladder.
pub fn song_title_only(title: &str) -> String {
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

    // Drop a trailing alias or credit field. YouTube Music appends an English
    // or romanised alias to non-English titles — `法螺話 - Tall Story`,
    // `キャラクターT - Character T` — and lrclib usually stores just the
    // original, so the combined form matches nothing. Japanese uploads also use
    // `Song / Artist`.
    const FIELD_SEPARATORS: &[&str] = &[" - ", " – ", " — ", " / ", "／", " ・ "];
    if let Some(cut) = FIELD_SEPARATORS
        .iter()
        .filter_map(|sep| out.find(sep))
        .min()
    {
        out.truncate(cut);
    }

    out.trim().to_string()
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
    artist
        .split(&[',', ';'][..])
        .next()
        .unwrap_or(artist)
        .trim()
        .to_string()
}

/// One LRCLIB query in the broadening ladder.
#[derive(Debug, PartialEq, Eq, Hash)]
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
        let song = song_title_only(&self.title);

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
        ladder.retain(|a| seen.insert(format!("{a:?}")));
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

    pub fn synced_lines(&self) -> Option<&[LyricLine]> {
        match &self.kind {
            LyricsKind::Synced(lines) => Some(lines),
            _ => None,
        }
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
            track_name: l.track_name,
            artist_name: l.artist_name,
            album_name: l.album_name,
            duration: l.duration,
            kind,
        })
    }
}

// ── ranking ──────────────────────────────────────────────────────────────────

/// Orders candidates best-first: synced before plain, then closest duration,
/// then artist/title agreement, then LRCLIB's own relevance order.
///
/// Records more than [`MAX_DURATION_DELTA`] from a known duration are dropped —
/// unless that would empty the list, in which case they are kept, so this never
/// turns "a poor match" into "no lyrics".
pub fn rank(mut items: Vec<TrackLyrics>, q: &LyricsQuery) -> Vec<TrackLyrics> {
    // Drop anything whose length doesn't match the track. Records with no
    // duration at all go too: we can't tell whether they fit.
    if q.duration.is_some() {
        items.retain(|c| {
            c.duration_delta(q.duration)
                .is_some_and(|d| d <= MAX_DURATION_DELTA)
        });
    }

    // Compare against the normalised forms: when the match came from a
    // broadened query, the raw title still carries the decoration that made the
    // precise query fail, so it would never compare equal.
    let title = strip_title_noise(&q.title).to_lowercase();
    let artist = primary_artist(&q.artist).to_lowercase();

    // Stable, so equal keys preserve LRCLIB's relevance ordering.
    items.sort_by(|a, b| {
        let key = |c: &TrackLyrics| {
            (
                // Synced first — an unsynced match is a worse experience than a
                // slightly mistimed synced one.
                !c.is_synced(),
                // Then closest length. Rounded to the second so a 0.4s
                // difference doesn't outweigh an artist match. Records with no
                // duration sort last rather than being treated as a perfect
                // match, which is what `unwrap_or(0.0)` used to do.
                c.duration_delta(q.duration)
                    .map_or(f64::INFINITY, |d| d.round()),
                !c.artist_name.to_lowercase().contains(&artist),
                !c.track_name.to_lowercase().eq(&title),
            )
        };
        let (a_sync, a_delta, a_art, a_tit) = key(a);
        let (b_sync, b_delta, b_art, b_tit) = key(b);
        a_sync
            .cmp(&b_sync)
            .then(a_delta.total_cmp(&b_delta))
            .then(a_art.cmp(&b_art))
            .then(a_tit.cmp(&b_tit))
    });

    items
}

// ── service ──────────────────────────────────────────────────────────────────

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
        match self.client.get_by_id(id).await {
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
            match self
                .client
                .get(&q.title, &q.artist, &q.album, duration)
                .await
            {
                Ok(l) => {
                    if let Some(found) = TrackLyrics::from_record(l) {
                        let close = found
                            .duration_delta(q.duration)
                            .is_some_and(|d| d <= EXACT_DURATION_DELTA);
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
        // things, and the UI shows them differently. Only reach for the plain
        // fallback once we know the search itself succeeded.
        let mut found = match self.search_first_match(q).await {
            Ok(found) => found,
            Err(e) if plain_fallback.is_some() => {
                log::warn!("lyrics search failed ({e}) — using the exact plain match");
                return Ok(plain_fallback);
            }
            Err(e) => return Err(e),
        };

        // Fold the exact-lookup hit into the pool and re-rank, so whichever
        // record's length is closest to the track wins — that hit is often not
        // the best-timed one available.
        if let Some(exact) = exact {
            if !found.iter().any(|c| c.id == exact.id) {
                found.push(exact);
            }
            found = rank(found, q);
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
            } => self.client.search_by_meta(track, artist, album).await,
            Attempt::FreeText(query) => self.client.search(query).await,
        }
    }

    /// Walks the ladder for the automatic match, stopping as soon as a rung
    /// offers *synced* lyrics.
    ///
    /// Stopping at the first rung with any usable record isn't enough: a
    /// precise rung often returns a pile of unsynced uploads while the one
    /// synced transcription sits under different metadata — a blank album, say
    /// — and is only reachable from a broader rung. Unsynced hits are therefore
    /// remembered and the search continues, so a timed transcription always
    /// wins over an untimed one.
    ///
    /// The cost lands on tracks that genuinely have no synced lyrics: those
    /// walk the whole ladder. Results are cached per track, so it is paid once.
    async fn search_first_match(&self, q: &LyricsQuery) -> Result<Vec<TrackLyrics>> {
        let mut last_err = None;
        let mut plain: Vec<TrackLyrics> = Vec::new();

        for attempt in q.search_ladder() {
            if matches!(&attempt, Attempt::FreeText(s) if s.is_empty()) {
                continue;
            }
            match self.run(&attempt).await {
                Ok(raw) => {
                    // A response whose records all lack lyrics still counts as
                    // a miss — keep broadening rather than reporting "no lyrics
                    // found" as the old `raw.is_empty()` check did.
                    let usable: Vec<TrackLyrics> = raw
                        .into_iter()
                        .filter_map(TrackLyrics::from_record)
                        .collect();
                    let ranked = rank(usable, q);

                    // `rank` sorts synced first, so the head tells us whether
                    // this rung has any.
                    if ranked.first().is_some_and(TrackLyrics::is_synced) {
                        log::debug!("lyrics: synced match on {attempt:?}");
                        return Ok(ranked);
                    }
                    if plain.is_empty() && !ranked.is_empty() {
                        log::debug!("lyrics: unsynced match on {attempt:?}, still looking");
                        plain = ranked;
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
            Some(e) if plain.is_empty() => Err(e.into()),
            _ => Ok(plain),
        }
    }

    /// Every candidate the ladder can reach, de-duplicated and ranked.
    ///
    /// Unlike the automatic match this runs *all* rungs and merges them, so the
    /// picker offers everything a manual lrclib search would turn up — stopping
    /// at the first rung meant a precise query returning one record hid the
    /// dozen a broader one would have found. It is only invoked when the user
    /// presses `c`, so the extra requests are paid for deliberately.
    ///
    /// Returns *full* records — LRCLIB's search response already includes
    /// `syncedLyrics`, so committing a choice needs no further request.
    pub async fn candidates(&self, q: &LyricsQuery) -> Result<Vec<TrackLyrics>> {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut all: Vec<TrackLyrics> = Vec::new();
        let mut last_err = None;

        for attempt in q.search_ladder() {
            if matches!(&attempt, Attempt::FreeText(s) if s.is_empty()) {
                continue;
            }
            match self.run(&attempt).await {
                Ok(raw) => {
                    for found in raw.into_iter().filter_map(TrackLyrics::from_record) {
                        if seen.insert(found.id) {
                            all.push(found);
                        }
                    }
                }
                Err(e) => {
                    log::warn!("lyrics: {attempt:?} failed: {e}");
                    last_err = Some(e);
                }
            }
        }

        if all.is_empty()
            && let Some(e) = last_err
        {
            return Err(e.into());
        }
        log::debug!("lyrics: {} candidates across the ladder", all.len());
        Ok(rank(all, q))
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
    tx: Sender<LyricsMsg>,
) {
    handle.spawn(async move {
        let result = svc.candidates(&query).await.map_err(|e| e.to_string());
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
            song_title_only("【歌ってみた】人マニア / covered by ヰ世界情緒"),
            "人マニア"
        );
        assert_eq!(
            song_title_only("人マニア / covered by ヰ世界情緒"),
            "人マニア"
        );
        assert_eq!(song_title_only("Song (Cover)"), "Song");
        assert_eq!(song_title_only("Song 【cover】"), "Song");
        assert_eq!(song_title_only("Song - covered by Someone"), "Song");
        assert_eq!(song_title_only("「歌ってみた」Song"), "Song");
        // `Song / Artist` is a common Japanese upload convention.
        assert_eq!(song_title_only("人マニア / ヰ世界情緒"), "人マニア");
    }

    #[test]
    fn strips_japanese_version_credits() {
        assert_eq!(song_title_only("ダーリン ver.わかばやし"), "ダーリン");
        assert_eq!(song_title_only("ダーリンver.わかばやし"), "ダーリン");
        assert_eq!(song_title_only("Song ver.Someone"), "Song");
        assert_eq!(song_title_only("Song ver: Someone"), "Song");
    }

    #[test]
    fn version_marker_does_not_fire_mid_word() {
        // "Cover." contains "ver." — cutting there would leave "Co".
        assert_eq!(song_title_only("Cover.jp Anthem"), "Cover.jp Anthem");
        assert_eq!(song_title_only("Discover.me"), "Discover.me");
    }

    #[test]
    fn strips_the_english_alias_youtube_music_appends() {
        // Real titles from the app log — lrclib stores only the original.
        assert_eq!(song_title_only("法螺話 - Tall Story"), "法螺話");
        assert_eq!(
            song_title_only("キャラクターT - Character T (feat. Kasane Teto)"),
            "キャラクターT"
        );
        assert_eq!(song_title_only("泡沫 - Utakata"), "泡沫");
        assert_eq!(song_title_only("千鳥 - Plover"), "千鳥");
        assert_eq!(song_title_only("食虫植物 - Carnivorous Plant"), "食虫植物");
        assert_eq!(
            song_title_only("ハナタバ - Hanataba (feat. KAFU)"),
            "ハナタバ"
        );
        assert_eq!(
            song_title_only("マインドブランド - Mind brand"),
            "マインドブランド"
        );
        // Punctuation inside the original title is part of it.
        assert_eq!(
            song_title_only("フィクションです。 - It’s Fiction."),
            "フィクションです。"
        );
        // A parenthesised alias is kept — only the trailing credit is cut.
        assert_eq!(
            song_title_only("逆さ月 (Reverse Moon) feat. asmi"),
            "逆さ月 (Reverse Moon)"
        );
    }

    #[test]
    fn hyphen_split_needs_surrounding_spaces() {
        // A hyphenated word is not a field separator.
        assert_eq!(song_title_only("Twenty-One"), "Twenty-One");
        assert_eq!(song_title_only("Re-Education"), "Re-Education");
    }

    #[test]
    fn strips_a_bare_by_credit_only_on_covers() {
        // Real title from the app log.
        assert_eq!(
            song_title_only("【歌ってみた】ドゥーマー by 花譜"),
            "ドゥーマー"
        );
        assert_eq!(song_title_only("Song (cover) by Someone"), "Song");

        // Without a cover marker, `by` is part of the name.
        assert_eq!(song_title_only("Stand By Me"), "Stand By Me");
        assert_eq!(song_title_only("Get By"), "Get By");
        assert_eq!(song_title_only("Drive By"), "Drive By");
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
        assert_eq!(song_title_only("Stand By Me feat. Someone"), "Stand By Me");
        assert_eq!(
            song_title_only("Discovery by Daft Punk"),
            "Discovery by Daft Punk"
        );
    }

    #[test]
    fn song_title_only_leaves_ordinary_titles_alone() {
        assert_eq!(song_title_only("Ribs"), "Ribs");
        assert_eq!(song_title_only("法螺話"), "法螺話");
        assert_eq!(song_title_only(""), "");
        // A kept multi-byte bracket group must not slice mid-character:
        // `】` is three bytes, so an inclusive range ends inside it.
        assert_eq!(
            strip_title_noise("【あいうえお】Song"),
            "【あいうえお】Song"
        );
        assert_eq!(song_title_only("「そら」Song"), "「そら」Song");
        // No panic on unbalanced or odd input.
        assert_eq!(song_title_only("Song (cover"), "Song (cover");
        song_title_only("【】");
        song_title_only("///");
        song_title_only("【】【】(((");
    }

    #[test]
    fn cover_reduction_is_stricter_than_noise_stripping() {
        // strip_title_noise is used by the precise steps and must preserve a
        // qualifier that denotes a different recording; song_title_only is the
        // broad fallback and may discard it.
        let t = "法螺話(self cover)";
        assert_eq!(strip_title_noise(t), t);
        assert_eq!(song_title_only(t), "法螺話");
    }

    #[test]
    fn normalises_youtube_artist_forms() {
        // YouTube's auto-generated channels are named "<artist> - Topic".
        assert_eq!(primary_artist("理芽 - Topic"), "理芽");
        // lrclib stores one artist per record, so a joined list matches nothing.
        assert_eq!(primary_artist("理芽, Guiano"), "理芽");
        assert_eq!(primary_artist("Lorde"), "Lorde");
        assert_eq!(primary_artist(""), "");
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
        assert!(svc.candidates(&q).await.expect("search errored").len() > 1);
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
            delta <= EXACT_DURATION_DELTA,
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

        let first = svc.search_first_match(&q).await.expect("search errored");
        let all = svc.candidates(&q).await.expect("search errored");
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
        // lrclib has three records: two at 174s and one at 181s. The 181s one
        // is outside the duration window, so two are offered.
        let all = svc.candidates(&q).await.expect("search errored");
        assert_eq!(all.len(), 2, "got {all:?}");
        assert!(
            all.iter().all(|c| c
                .duration_delta(q.duration)
                .is_some_and(|d| d <= MAX_DURATION_DELTA)),
            "an out-of-window record was offered"
        );

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
                svc.candidates(&q).await.expect("search errored").len() > 1,
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

        let found = svc.candidates(&query).await.expect("search failed");
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
        assert_eq!(ids(&out), [1]);

        // With no known duration to compare against, it is kept and ordered
        // behind nothing in particular.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        assert_eq!(rank(vec![unknown, rec(1, 250.0, true)], &q(None)).len(), 2);
    }

    #[test]
    fn a_record_without_duration_is_hidden_when_the_track_length_is_known() {
        // We can't tell whether it fits, so it can't be vouched for.
        let mut unknown = rec(9, 0.0, true);
        unknown.duration = None;
        assert!(rank(vec![unknown], &q(Some(244.0))).is_empty());

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
    fn far_off_records_are_hidden_even_when_nothing_else_matches() {
        // These used to be kept as a last resort. A lyric sheet for a different
        // edit is worse than none: it scrolls visibly out of time.
        let out = rank(
            vec![rec(1, 400.0, true), rec(2, 500.0, true)],
            &q(Some(245.0)),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn the_window_is_a_few_seconds_not_a_dozen() {
        // A ~15s difference used to pass; that is what prompted tightening it.
        let out = rank(
            vec![rec(1, 259.0, true), rec(2, 248.0, true)],
            &q(Some(244.0)),
        );
        assert_eq!(ids(&out), [2], "a 15s difference must not be offered");
    }

    #[test]
    fn unknown_duration_skips_the_filter() {
        let out = rank(vec![rec(1, 400.0, true), rec(2, 10.0, true)], &q(None));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ties_preserve_source_order() {
        let out = rank(
            vec![
                rec(7, 245.0, true),
                rec(8, 245.0, true),
                rec(9, 245.0, true),
            ],
            &q(Some(245.0)),
        );
        assert_eq!(ids(&out), [7, 8, 9]);
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
