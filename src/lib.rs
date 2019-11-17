// Mindec -- Music metadata indexer
// Copyright 2017 Ruud van Asseldonk
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

// TODO: Remove once near-stable.
#![allow(dead_code)]

extern crate claxon;
extern crate crossbeam;
extern crate libc;
extern crate serde_json;
extern crate unicode_normalization;

mod flat_tree; // TODO: Rename.

pub mod net;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::collections::btree_map;
use std::fmt;
use std::io;
use std::io::Write;
use std::mem;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::u32;
use std::u64;

use unicode_normalization::UnicodeNormalization;

// Stats of my personal music library at this point:
//
//     11.5k tracks
//      1.2k albums
//      0.3k album artists
//      1.4k track artists
//
// The observation is that there is an order of magnitude difference between
// the track count and album count, and also between album count and artist
// count. In other words, track data will dominate, and album artist data is
// hardly relevant.
//
// What should I design for? My library will probably grow to twice its size
// over time. Perhaps even to 10x the size. But I am pretty confident that it
// will not grow by 100x. So by designing the system to support 1M tracks, I
// should be safe.
//
// Let's consider IDs for a moment. The 16-byte MusicBrainz UUIDs take up a lot
// of space, and I want to run on low-end systems, in particular the
// first-generation Raspberry Pi, which has 16k L1 cache and 128k L2 cache.
// Saving 50% on IDs can have a big impact there. So under the above assumptions
// of 1M tracks, can I get away with using 8 bytes of the 16-byte UUIDs? Let's
// consider the collision probability. With 8-byte identifiers, to have a 1%
// collision probability, one would need about 608M tracks. That is a lot more
// than what I am designing for. For MusicBrainz, which aims to catalog every
// track ever produced by humanity, this might be too risky. But for my personal
// collection the memory savings are well worth the risk.
//
// Let's dig a bit further: I really only need to uniquely identify album
// artists, then albums by that artist, and then tracks on those albums. And I
// would like to do so based on their metadata only, not involving global
// counters, because I want something that is deterministic but which can be
// parallelized. So how many bits do we need for the album artist? Let's say
// the upper bound is 50k artists, and I want a collision probability of at most
// 0.1% at that number of artists. The lowest multiple of 8 that I can get away
// with is 48 bits.

#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackId(u64);

// TODO: Field should not be pub.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AlbumId(pub u64);

// TODO: Field should not be pub.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArtistId(pub u64);

/// Index into a byte array that contains length-prefixed strings.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StringRef(u32);

/// Index into a byte array that contains length-prefixed strings.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FilenameRef(u32);

impl TrackId {
    #[inline]
    pub fn parse(src: &str) -> Option<TrackId> {
        u64::from_str_radix(src, 16).ok().map(TrackId)
    }
}

impl AlbumId {
    #[inline]
    pub fn parse(src: &str) -> Option<AlbumId> {
        u64::from_str_radix(src, 16).ok().map(AlbumId)
    }
}

impl ArtistId {
    #[inline]
    pub fn parse(src: &str) -> Option<ArtistId> {
        u64::from_str_radix(src, 16).ok().map(ArtistId)
    }
}

#[repr(C)]
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Track {
    pub album_id: AlbumId,
    pub title: StringRef,
    pub artist: StringRef,
    pub filename: FilenameRef,
    // Using u16 for duration gives us a little over 18 hours as maximum
    // duration; using u8 for track number gives us at most 255 tracks. This is
    // perhaps a bit limiting, but it does allow us to squeeze a `(TrackId,
    // Track)` into half a cache line, so they never straddle cache line
    // boundaries. And of course more of them fit in the cache. If range ever
    // becomes a problem, we could use some of the disc number bits to extend
    // the duration range or track number range.
    pub duration_seconds: u16,
    pub disc_number: u8,
    pub track_number: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl Date {
    pub fn new(year: u16, month: u8, day: u8) -> Date {
        // We assume dates are parsed from YYYY-MM-DD strings.
        // Note that zeros are valid, they are used to indicate
        // unknown months or days.
        debug_assert!(year <= 9999);
        debug_assert!(month <= 12);
        debug_assert!(day <= 31);
        Date {
            year,
            month,
            day,
        }
    }
}

#[repr(C)]
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Album {
    pub artist_id: ArtistId,
    pub title: StringRef,
    pub original_release_date: Date,
}

#[repr(C)]
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Artist {
    pub name: StringRef,
    pub name_for_sort: StringRef,
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:04}", self.year)?;
        if self.month == 0 { return Ok(()) }
        write!(f, "-{:02}", self.month)?;
        if self.day == 0 { return Ok(()) }
        write!(f, "-{:02}", self.day)
    }
}

pub trait MetaIndex {
    /// Return the number of tracks in the index.
    fn len(&self) -> usize;

    /// Resolve a `StringRef` to a string slice.
    ///
    /// Returns an empty string if the ref is out of bounds. May return a
    /// garbage string when the ref is at the wrong offset.
    fn get_string(&self, sr: StringRef) -> &str;

    /// Resolve a `StringRef` to a filename string slice.
    ///
    /// Same as `get_string()`, but for filenames, which have a different arena.
    fn get_filename(&self, sr: FilenameRef) -> &str;

    /// Return track metadata.
    fn get_track(&self, id: TrackId) -> Option<&Track>;

    /// Return album metadata.
    fn get_album(&self, id: AlbumId) -> Option<&Album>;

    /// Return all tracks that are part of the album.
    fn get_album_tracks(&self, id: AlbumId) -> &[(TrackId, Track)];

    /// Return all tracks, ordered by id.
    fn get_tracks(&self) -> &[(TrackId, Track)];

    /// Return all albums, ordered by id.
    fn get_albums(&self) -> &[(AlbumId, Album)];

    /// Return all album artists, ordered by id.
    fn get_artists(&self) -> &[(ArtistId, Artist)];

    /// Look up an artist by id.
    fn get_artist(&self, ArtistId) -> Option<&Artist>;

    /// Write a json representation of the album list to the writer.
    fn write_albums_json<W: Write>(&self, mut w: W) -> io::Result<()> {
        write!(w, "[")?;
        let mut first = true;
        for &(ref id, ref album) in self.get_albums() {
            // The unwrap is safe here, in the sense that if the index is
            // well-formed, it will never fail. The id is provided by the index
            // itself, not user input, so the artist should be present.
            let artist = self.get_artist(album.artist_id).unwrap();
            if !first { write!(w, ",")?; }
            write!(w, r#"{{"id":"{}","title":"#, id)?;
            serde_json::to_writer(&mut w, self.get_string(album.title))?;
            write!(w, r#","artist":"#)?;
            serde_json::to_writer(&mut w, self.get_string(artist.name))?;
            write!(w, r#","sort_artist":"#)?;
            serde_json::to_writer(&mut w, self.get_string(artist.name_for_sort))?;
            write!(w, r#","date":"{}"}}"#, album.original_release_date)?;
            first = false;
        }
        write!(w, "]")
    }

    /// Write a json representation of the album and its tracks to the writer.
    ///
    /// The album is expected to come from this index, so the artists and
    /// strings it references are valid.
    fn write_album_json<W: Write>(&self, mut w: W, id: AlbumId, album: &Album) -> io::Result<()> {
        // The unwrap is safe here, in the sense that if the index is
        // well-formed, it will never fail. The id is provided by the index
        // itself, not user input, so the artist should be present.
        let artist = self.get_artist(album.artist_id).unwrap();

        write!(w, r#"{{"title":"#)?;
        serde_json::to_writer(&mut w, self.get_string(album.title))?;
        write!(w, r#","artist":"#)?;
        serde_json::to_writer(&mut w, self.get_string(artist.name))?;
        write!(w, r#","sort_artist":"#)?;
        serde_json::to_writer(&mut w, self.get_string(artist.name_for_sort))?;
        write!(w, r#","date":"{}","tracks":["#, album.original_release_date)?;
        let mut first = true;
        for &(ref tid, ref track) in self.get_album_tracks(id) {
            if !first { write!(w, ",")?; }
            write!(w, r#"{{"id":"{}","disc_number":{},"track_number":{},"title":"#,
                   tid, track.disc_number, track.track_number)?;
            serde_json::to_writer(&mut w, self.get_string(track.title))?;
            write!(w, r#","artist":"#)?;
            serde_json::to_writer(&mut w, self.get_string(track.artist))?;
            write!(w, r#","duration_seconds":{}}}"#, track.duration_seconds)?;
            first = false;
        }
        write!(w, "]}}")
    }
}

#[derive(Debug)]
pub enum Error {
    /// An IO error during writing the index or reading the index or metadata.
    IoError(io::Error),

    /// An FLAC file to be indexed could not be read.
    FormatError(claxon::Error),
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::IoError(err)
    }
}

impl From<claxon::Error> for Error {
    fn from(err: claxon::Error) -> Error {
        match err {
            claxon::Error::IoError(io_err) => Error::IoError(io_err),
            _ => Error::FormatError(err),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

#[test]
fn struct_sizes_are_as_expected() {
    use std::mem;
    assert_eq!(mem::size_of::<Track>(), 24);
    assert_eq!(mem::size_of::<Album>(), 16);
    assert_eq!(mem::size_of::<Artist>(), 8);
    assert_eq!(mem::size_of::<(TrackId, Track)>(), 32);

    assert_eq!(mem::align_of::<Track>(), 8);
    assert_eq!(mem::align_of::<Album>(), 8);
    assert_eq!(mem::align_of::<Artist>(), 4);
}

#[derive(Clone, Debug)]
pub enum IssueDetail {
    /// A required metadata field is missing. Contains the field name.
    FieldMissingError(&'static str),

    /// A metadata field could be parsed. Contains the field name.
    FieldParseFailedError(&'static str),

    /// Two different titles were found for albums with the same mbid.
    /// Contains the title used, and the discarded alternative.
    AlbumTitleMismatch(AlbumId, String, String),

    /// Two different release dates were found for albums with the same mbid.
    /// Contains the date used, and the discarded alternative.
    AlbumReleaseDateMismatch(AlbumId, Date, Date),

    /// Two different artists were found for albums with the same mbid.
    /// Contains the artist used, and the discarded alternative.
    AlbumArtistMismatch(AlbumId, String, String),

    /// Two different names were found for album artists with the same mbid.
    /// Contains the name used, and the discarded alternative.
    ArtistNameMismatch(ArtistId, String, String),

    /// Two different sort names were found for album artists with the same mbid.
    /// Contains the name used, and the discarded alternative.
    ArtistSortNameMismatch(ArtistId, String, String),
}

impl IssueDetail {
    pub fn for_file(self, filename: String) -> Issue {
        Issue {
            filename: filename,
            detail: self,
        }
    }
}

#[derive(Debug)]
pub struct Issue {
    pub filename: String,
    pub detail: IssueDetail,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:\n  ", self.filename)?;
        match self.detail {
            IssueDetail::FieldMissingError(field) =>
                write!(f, "error: field '{}' missing.", field),
            IssueDetail::FieldParseFailedError(field) =>
                write!(f, "error: failed to parse field '{}'.", field),
            IssueDetail::AlbumTitleMismatch(_id, ref title, ref alt) =>
                write!(f, "warning: discarded inconsistent album title '{}' in favour of '{}'.", alt, title),
            IssueDetail::AlbumReleaseDateMismatch(_id, ref date, ref alt) =>
                write!(f, "warning: discarded inconsistent album release date {} in favour of {}.", alt, date),
            IssueDetail::AlbumArtistMismatch(_id, ref artist, ref alt) =>
                write!(f, "warning: discarded inconsistent album artist '{}' in favour of '{}'.", alt, artist),
            IssueDetail::ArtistNameMismatch(_id, ref name, ref alt) =>
                write!(f, "warning: discarded inconsistent artist name '{}' in favour of '{}'.", alt, name),
            IssueDetail::ArtistSortNameMismatch(_id, ref sort_name, ref alt) =>
                write!(f, "warning: discarded inconsistent sort name '{}' in favour of '{}'.", alt, sort_name),
        }
    }
}

#[derive(Debug)]
pub enum Progress {
    /// A number of files have been indexed.
    Indexed(u32),
    /// An issue with a file was encountered.
    Issue(Issue),
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Display for AlbumId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Display for ArtistId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

struct StringDeduper {
    pub strings_to_id: HashMap<String, u32>,
    pub strings: Vec<String>,
}

impl StringDeduper {
    pub fn new() -> StringDeduper {
        StringDeduper {
            strings_to_id: HashMap::new(),
            strings: Vec::new(),
        }
    }

    /// Insert the string, or return its index if it was present already.
    pub fn insert(&mut self, string: &str) -> u32 {
        // If the string exists already, return its id, otherwise insert it.
        // This does involve two lookups in the case of insert, but it does save
        // an allocation that turns the &str into a String when an insert is
        // required. We expect inserts to occur less than half of the time
        // (usually the sort artist is the same as the artist, and many tracks
        // share the same artist), therefore opt for the check first. Empirical
        // evidence: on my personal library, about 22% of the strings need to be
        // inserted (12.6k out of 57.8k total strings).
        let next_id = self.strings.len() as u32;
        // TODO: Unicode-normalize the string.
        if let Some(id) = self.strings_to_id.get(string) { return *id }
        self.strings_to_id.insert(string.to_string(), next_id);
        self.strings.push(string.to_string());
        next_id
    }

    /// Return the underlying string vector, destroying the deduplicator.
    pub fn into_vec(self) -> Vec<String> {
        self.strings
    }

    /// Return the string with the given index. Panics when out of bounds.
    pub fn get(&self, index: u32) -> &str {
        &self.strings[index as usize]
    }

    /// Replace most straigt quotes (') in strings with typographer’s quotes (‘ and ’).
    ///
    /// Although some tags use typographer’s quotes, most do not, also on
    /// Musicbrainz. But the typographer’s quotes look nicer, especially in Work
    /// Sans, which is used for Mindec’s library browser. So apply a heuristic
    /// to replace most straigh quotes with curly ones.
    ///
    /// This is a heuristic, it is not perfect. In particular, this function
    /// mistakes apostrophes before a word, for opening quotes. The tags must be
    /// edited to sidestep this shortcoming.
    pub fn upgrade_quotes(&mut self) {
        for s in self.strings.iter_mut() {
            // NOTE: We could use memchr for this if it turns out to be a
            // bottleneck.
            let mut from = 0;
            while let Some(off) = &s[from..].find("'") {
                let i = from + off;

                let before = if i > 0 { s.as_bytes()[i - 1] } else { b' ' };
                let after = if i < s.len() - 1 { s.as_bytes()[i + 1] } else { b' ' };

                let after_word = after == b' ' || after == b',' || after == b')';
                let after_letter = before.is_ascii_alphabetic();
                let after_digit = before.is_ascii_digit();
                let before_word = before == b' ';
                let before_letter = after.is_ascii_alphabetic();
                let before_digit = after.is_ascii_digit();

                let replacement = match () {
                    // Contractions like n’t, names like O’Neil.
                    _ if after_letter && before_letter => Some("’"),
                    // Abbreviations like dreamin’.
                    _ if after_letter && after_word => Some("’"),
                    // Usually years or other numbers, like 80’s or 5’s.
                    _ if after_digit && before_letter => Some("’"),
                    // Usually years, like ’93.
                    _ if before_word && before_digit => Some("’"),
                    // Often opening quote, but it can also be a contraction,
                    // like ’cause, ’em, or ’til, and then this gets it wrong
                    // ... To remove all doubt, your tags.
                    _ if before_word && before_letter => Some("‘"),
                    // What remains in my collection are things like contractions
                    // in non-ascii words (e.g. C’était), and quotes after
                    // numbers, which I think stands for a length in feet.
                    // Non-ascii letters are difficult to detect, and for the
                    // numbers, the straight quote is appropriate, so we'll
                    // leave it at this.
                    _ => None
                };

                if let Some(r) = replacement {
                    s.replace_range(i..i + 1, r);
                    from = i + r.len();
                } else {
                    from = i + "'".len();
                }
            }
        }
    }
}

/// Return an issue if the two albums are not equal.
fn albums_different(
    strings: &StringDeduper,
    id: AlbumId,
    a: &Album,
    b: &Album)
    -> Option<IssueDetail>
{
    let title_a = strings.get(a.title.0);
    let title_b = strings.get(b.title.0);

    if title_a != title_b {
        return Some(IssueDetail::AlbumTitleMismatch(
            id,
            title_a.into(),
            title_b.into(),
        ));
    }

    if a.original_release_date != b.original_release_date {
        return Some(IssueDetail::AlbumReleaseDateMismatch(
            id,
            a.original_release_date,
            b.original_release_date,
        ));
    }

    if a.artist_id != b.artist_id {
        unimplemented!("TODO: Look up artist names.");
    }

    None
}

/// Return an issue if the two artists are not equal.
fn artists_different(
    strings: &StringDeduper,
    id: ArtistId,
    a: &Artist,
    b: &Artist)
    -> Option<IssueDetail>
{
    let name_a = strings.get(a.name.0);
    let name_b = strings.get(b.name.0);
    let sort_name_a = strings.get(a.name_for_sort.0);
    let sort_name_b = strings.get(b.name_for_sort.0);

    if name_a != name_b {
        return Some(IssueDetail::ArtistNameMismatch(
            id,
            name_a.into(),
            name_b.into(),
        ));
    }

    if sort_name_a != sort_name_b {
        return Some(IssueDetail::ArtistSortNameMismatch(
            id,
            sort_name_a.into(),
            sort_name_b.into(),
        ));
    }

    None
}

struct BuildMetaIndex {
    artists: BTreeMap<ArtistId, Artist>,
    albums: BTreeMap<AlbumId, Album>,
    tracks: BTreeMap<TrackId, Track>,
    strings: StringDeduper,
    filenames: Vec<String>,
    words_track_title: BTreeSet<(String, TrackId)>,
    words_album_title: BTreeSet<(String, AlbumId)>,
    words_album_artist: BTreeSet<(String, ArtistId)>,

    // When the track artist differs from the album artist, the words that occur
    // in the track artist but not in the album artist, are included here.
    words_track_artist: BTreeSet<(String, TrackId)>,

    // For album and artist data, store the file from which the metadata was
    // taken. This is later used for error reporting when the builders are
    // merged. If there are inconsistencies at that point, we need to be able to
    // attribute the album or artist metadata in this builder to a specific
    // file. If all files agree then it's an arbitrary one, but if there was
    // already inconsistent data in this builder's input, we need to remember
    // which file we chose. Values are indices into the `filenames` vector.
    album_sources: HashMap<AlbumId, FilenameRef>,
    artist_sources: HashMap<ArtistId, FilenameRef>,

    // TODO: This option, to drop it when processing is done, is a bit of a
    // hack. It would be nice to not have it in the builder at all.
    progress: Option<SyncSender<Progress>>,
}

fn parse_date(date_str: &str) -> Option<Date> {
    // We expect at least a year.
    if date_str.len() < 4 { return None }

    let year = u16::from_str(&date_str[0..4]).ok()?;
    let mut month: u8 = 0;
    let mut day: u8 = 0;

    // If there is something following the year, it must be dash, and there must
    // be at least two digits for the month.
    if date_str.len() > 4 {
        if date_str.as_bytes()[4] != b'-' { return None }
        if date_str.len() < 7 { return None }
        month = u8::from_str(&date_str[5..7]).ok()?;
    }

    // If there is something following the month, it must be dash, and there
    // must be exactly two digits for the day.
    if date_str.len() > 7 {
        if date_str.as_bytes()[7] != b'-' { return None }
        if date_str.len() != 10 { return None }
        day = u8::from_str(&date_str[8..10]).ok()?;
    }

    // This is not the most strict date well-formedness check that we can do,
    // but it is something at least. Note that we do allow the month and day to
    // be zero, to indicate the entire month or entire year.
    if month > 12 || day > 31 {
        return None
    }

    Some(Date::new(year, month, day))
}

fn parse_uuid(uuid: &str) -> Option<u64> {
    // Validate that the textual format of the UUID is as expected.
    // E.g. `1070cbb2-ad74-44ce-90a4-7fa1dfd8164e`.
    if uuid.len() != 36 { return None }
    if uuid.as_bytes()[8] != b'-' { return None }
    if uuid.as_bytes()[13] != b'-' { return None }
    if uuid.as_bytes()[18] != b'-' { return None }
    if uuid.as_bytes()[23] != b'-' { return None }
    // We parse the first and last 4 bytes and use these as the 8-byte id.
    // See the comments above for the motivation for using only 64 of the 128
    // bits. We take the front and back of the string because it is easy, there
    // are no dashes to strip. Also, the non-random version bits are in the
    // middle, so this way we avoid using those.
    let high = u32::from_str_radix(&uuid[..8], 16).ok()? as u64;
    let low = u32::from_str_radix(&uuid[28..], 16).ok()? as u64;
    Some((high << 32) | low)
}

fn get_track_id(album_id: AlbumId,
                disc_number: u8,
                track_number: u8)
                -> TrackId {
    // Take the bits from the album id, so all the tracks within one album are
    // adjacent. This is desirable, because two tracks fit in a cache line,
    // halving the memory access cost of looking up an entire album. It also
    // makes memory access more predictable. Finally, if the 52 most significant
    // bits uniquely identify the album (which we assume), then all tracks are
    // guaranteed to be adjacent, and we can use an efficient range query to
    // find them.
    let high = album_id.0 & 0xffff_ffff_ffff_f000;

    // Finally, within an album the disc number and track number should uniquely
    // identify the track.
    let mid = ((disc_number & 0xf) as u64) << 8;
    let low = track_number as u64;

    TrackId(high | mid | low)
}

fn push_word(dest: &mut Vec<String>, word: &mut String) {
    if word.len() == 0 {
        return
    }

    let mut w = String::new();
    mem::swap(&mut w, word);
    dest.push(w);
}

/// Fills the vector with the words in the string in normalized form.
///
/// This first normalizes words to Unicode Normalization Form KD, which
/// decomposes characters with accents into the character and the accent
/// separately. The "KD" form, as opposed to the "D" form, also replaces more
/// things that have the same semantic meaning, such as replacing superscripts
/// with normal digits. Finally (not part of the KD normalization), everything
/// is lowercased, and accents and some punctuation are removed.
fn normalize_words(title: &str, dest: &mut Vec<String>) {
    // We assume that in the majority of the cases, the transformations
    // below do not change the number of bytes.
    let mut word = String::new();
    let mut num_dots = 0;

    // Drop some punctuation characters and accents. We remove punctuation that
    // is unlikely to contain a lot of information about the title. (Deadmau5
    // can go and use some normal titles next time.) We remove accents to make
    // searching easier without having to type the exact accent.
    let drop = "“”‘’'\"()[]«»,❦\u{300}\u{301}\u{302}\u{303}\u{304}\u{307}\u{308}\u{327}";
    let keep = "$€#&=*%∆";

    // Cut words at the following punctuation characters, but still include them
    // as a word of their own. This ensures that words are broken up properly,
    // but it still allows searching for this punctuation. This is important,
    // because some artists are under the illusion that it is cool to use
    // punctuation as part of a name.
    let cut = "/\\@_+-:!?<>";

    // Loop over the characters, normalized and lowercased.
    for ch in title.nfkd().flat_map(|nch| nch.to_lowercase()) {
        match ch {
            // Split words at whitespace or at the cut characters.
            _ if ch.is_whitespace() => {
                push_word(dest, &mut word);
            }
            _ if cut.contains(ch) => {
                push_word(dest, &mut word);
                dest.push(ch.to_string());
            }
            // The period is special: generally we don't want to include it as a
            // word, and simply ignore it altogether. (E.g. "S.P.Y" turns into
            // "spy".) But the ellipisis (...) we do want to keep. There are
            // even tracks titled "...". So we need to detect the ellipsis.
            '.' => {
                num_dots += 1;
                if num_dots == 3 {
                    dest.push("...".to_string());
                    word = String::new();
                }
                continue
            }
            // Normalize a few characters to more common ones.
            // Sometimes used in "n°", map to "no".
            '°' => word.push('o'),
            '♯' => word.push('#'),
            'ø' => word.push('o'),
            'æ' => word.push_str("ae"),
            'œ' => word.push_str("oe"),
            // A hyphen, use the ascii one instead.
            '\u{2010}' => word.push('-'),
            // I do want to be able to find my Justice albums with a normal
            // keyboard.
            '✝' => {
                push_word(dest, &mut word);
                dest.push("cross".to_string());
            }
            '∞' => {
                push_word(dest, &mut word);
                dest.push("infinity".to_string());
            }
            '¥' => {
                push_word(dest, &mut word);
                dest.push("yen".to_string());
            }
            // Drop characters that we don't care for, keep characters that we
            // definitely care for.
            _ if drop.contains(ch) => {}
            _ if keep.contains(ch) || ch.is_alphanumeric() => word.push(ch),
            _ => panic!("Unknown character {} ({}) in title: {}", ch, ch.escape_unicode(), title),
        }

        // Reset the ellipsis counter after every non-period character.
        num_dots = 0;
    }

    // Push the final word.
    push_word(dest, &mut word);
}

impl BuildMetaIndex {
    pub fn new(progress: SyncSender<Progress>) -> BuildMetaIndex {
        BuildMetaIndex {
            artists: BTreeMap::new(),
            albums: BTreeMap::new(),
            tracks: BTreeMap::new(),
            strings: StringDeduper::new(),
            filenames: Vec::new(),
            words_track_title: BTreeSet::new(),
            words_album_title: BTreeSet::new(),
            words_album_artist: BTreeSet::new(),
            words_track_artist: BTreeSet::new(),
            album_sources: HashMap::new(),
            artist_sources: HashMap::new(),
            progress: Some(progress),
        }
    }

    fn issue(&mut self, filename: String, detail: IssueDetail) {
        let issue = detail.for_file(filename);
        self.progress.as_mut().unwrap().send(Progress::Issue(issue)).unwrap();
    }

    fn error_missing_field(&mut self, filename: String, field: &'static str) {
        self.issue(filename, IssueDetail::FieldMissingError(field));
    }

    fn error_parse_failed(&mut self, filename: String, field: &'static str) {
        self.issue(filename, IssueDetail::FieldParseFailedError(field));
    }

    pub fn insert(
        &mut self,
        filename: &str,
        streaminfo: &claxon::metadata::StreamInfo,
        tags: &mut claxon::metadata::Tags
    ) {
        let mut disc_number = None;
        let mut track_number = None;
        let mut title = None;
        let mut album = None;
        let mut artist = None;
        let mut album_artist = None;
        let mut album_artist_for_sort = None;
        let mut date = None;
        let mut original_date = None;

        let mut mbid_album = 0;
        let mut mbid_artist = 0;

        let filename_id = self.filenames.len() as u32;
        let filename_string = filename.to_string();

        for (tag, value) in tags {
            match &tag.to_ascii_lowercase()[..] {
                // TODO: Replace unwraps here with proper parse error reporting.
                "album"                     => album = Some(self.strings.insert(value)),
                "albumartist"               => album_artist = Some(self.strings.insert(value)),
                "albumartistsort"           => album_artist_for_sort = Some(self.strings.insert(value)),
                "artist"                    => artist = Some(self.strings.insert(value)),
                "discnumber"                => disc_number = Some(u8::from_str(value).unwrap()),
                "musicbrainz_albumartistid" => mbid_artist = match parse_uuid(value) {
                    Some(id) => id,
                    None => return self.error_parse_failed(filename_string, "musicbrainz_albumartistid"),
                },
                "musicbrainz_albumid"       => mbid_album = match parse_uuid(value) {
                    Some(id) => id,
                    None => return self.error_parse_failed(filename_string, "musicbrainz_albumid"),
                },
                "originaldate"              => original_date = parse_date(value),
                "date"                      => date = parse_date(value),
                "title"                     => title = Some(self.strings.insert(value)),
                "tracknumber"               => track_number = Some(u8::from_str(value).unwrap()),
                _ => {}
            }
        }

        if mbid_album == 0 {
            return self.error_missing_field(filename_string, "musicbrainz_albumid")
        }
        if mbid_artist == 0 {
            return self.error_missing_field(filename_string, "musicbrainz_albumartistid")
        }

        // TODO: Make a macro for this, this is terrible.
        let f_disc_number = disc_number.unwrap_or(1);
        let f_track_number = match track_number {
            Some(t) => t,
            None => return self.error_missing_field(filename_string, "tracknumber"),
        };
        let f_title = match title {
            Some(t) => t,
            None => return self.error_missing_field(filename_string, "title"),
        };
        let f_track_artist = match artist {
            Some(a) => a,
            None => return self.error_missing_field(filename_string, "artist"),
        };
        let f_album = match album {
            Some(a) => a,
            None => return self.error_missing_field(filename_string, "album"),
        };
        let f_album_artist = match album_artist {
            Some(a) => a,
            None => return self.error_missing_field(filename_string, "albumartist"),
        };

        // Use the 'originaldate' field, fall back to 'date' if it is not set.
        let f_date = match original_date.or(date) {
            Some(d) => d,
            None => return self.error_missing_field(filename_string, "originaldate"),
        };

        let artist_id = ArtistId(mbid_artist);
        let album_id = AlbumId(mbid_album);
        let track_id = get_track_id(album_id, f_disc_number, f_track_number);

        // Split the title, album, and album artist, on words, and add those to
        // the indexes, to allow finding the track/album/artist later by word.
        let mut words = Vec::new();
        normalize_words(&self.strings.get(f_title), &mut words);
        for w in words.drain(..) { self.words_track_title.insert((w, track_id)); }
        normalize_words(&self.strings.get(f_album), &mut words);
        for w in words.drain(..) { self.words_album_title.insert((w, album_id)); }
        normalize_words(&self.strings.get(f_album_artist), &mut words);
        for w in words.drain(..) { self.words_album_artist.insert((w, artist_id)); }

        // Normalize the sort artist too. Generally, the only thing it is useful
        // for is to turn e.g. "The Who" into "Who, The". (Data from Musicbrainz
        // also puts the last name first for artists who use their real name,
        // but I dislike this.) But this is not sufficient for sorting alone:
        // there can still be case differences (e.g. "dEUS" and "deadmau5"
        // sorting last because they are lowercase) and accents (e.g. "Étienne
        // de Crécy" sorting last, and not with the "E"). The correct sort
        // ordering depends on locale. I am going to ignore all of that and turn
        // characters into the lowercase ascii character that looks most like
        // it, then sort by that.
        // TODO: Avoid inserting non-normalized sort artist in the string
        // deduplicator above; the non-normalized string is never referenced
        // except here temporarily.
        normalize_words(&self.strings.get(album_artist_for_sort.unwrap_or(f_album_artist)), &mut words);
        let sort_artist = words.join(" ");
        let f_album_artist_for_sort = self.strings.insert(&sort_artist);
        words.clear();

        // If the track artist differs from the album artist, add words for the
        // track artist, but only for the words that do not occur in the album
        // artist. This allows looking up e.g. a "feat. artist", without
        // polluting the index with every track by that artist.
        if f_track_artist != f_album_artist {
            normalize_words(&self.strings.get(f_track_artist), &mut words);
            for w in words.drain(..) {
                let pair = (w, artist_id);
                if !self.words_album_artist.contains(&pair) {
                    self.words_track_artist.insert((pair.0, track_id));
                }
            }
        }

        // TODO: Check for u16 overflow.
        // TODO: Warn if `streaminfo.samples` is None.
        let samples = streaminfo.samples.unwrap_or(0);
        let seconds = (samples + streaminfo.sample_rate as u64 / 2) / streaminfo.sample_rate as u64;

        let track = Track {
            album_id: album_id,
            disc_number: f_disc_number,
            track_number: f_track_number,
            title: StringRef(f_title),
            artist: StringRef(f_track_artist),
            duration_seconds: seconds as u16,
            filename: FilenameRef(filename_id),
        };
        let album = Album {
            artist_id: artist_id,
            title: StringRef(f_album),
            original_release_date: f_date,
        };
        let artist = Artist {
            name: StringRef(f_album_artist),
            name_for_sort: StringRef(f_album_artist_for_sort),
        };

        let mut add_album = true;
        let mut add_artist = true;

        // Check for consistency if duplicates occur.
        if self.tracks.get(&track_id).is_some() {
            // TODO: This should report an `Issue`, not panic.
            panic!("Duplicate track {}, file {}.", track_id, filename);
        }

        if let Some(existing_album) = self.albums.get(&album_id) {
            if let Some(detail) = albums_different(&self.strings, album_id, existing_album, &album) {
                let issue = detail.for_file(filename_string.clone());
                self.progress.as_mut().unwrap().send(Progress::Issue(issue)).unwrap();
            }
            add_album = false;
        }

        if let Some(existing_artist) = self.artists.get(&artist_id) {
            if let Some(detail) = artists_different(&self.strings, artist_id, existing_artist, &artist) {
                let issue = detail.for_file(filename_string.clone());
                self.progress.as_mut().unwrap().send(Progress::Issue(issue)).unwrap();
            }
            add_artist = false;
        }

        self.filenames.push(filename_string);
        self.tracks.insert(track_id, track);

        if add_album {
            self.albums.insert(album_id, album);
            self.album_sources.insert(album_id, FilenameRef(filename_id));
        }

        if add_artist {
            self.artists.insert(artist_id, artist);
            self.artist_sources.insert(artist_id, FilenameRef(filename_id));
        }
    }
}

/// Indices into a sorted array based on the most significant byte of an id.
///
/// This is a middle ground between storing an additional hash table, which
/// would require O(n) storage, but enable O(1) lookup of an artist, album, or
/// track, and the full binary search, which requires no additional storage,
/// but makes lookups O(log n).
///
/// A hash table would have two cache misses (one for the table, one for the
/// actual value). A binary search has log(n) cache misses (one for every try).
///
/// With the bookmarks, we store ranges into the full array indexed on the most
/// significant byte of the id. We spend 1028 additional bytes for the
/// bookmarks. Lookups are now O(log2(n) - 8). For 10k tracks, log2(n) is only
/// around 13, so we cut the majority of steps off of the binary search, and
/// with that also the cache misses. Furthermore, because the bookmarks table is
/// small unlike a full hash table, it is likely to be cached, so accessing it
/// is essentially free.
struct Bookmarks {
    bookmarks: Box<[u32; 257]>,
}

impl Bookmarks {
    pub fn new<I>(iter: I) -> Bookmarks where I: Iterator<Item = u64> {
        let mut bookmarks = [0; 257];
        let mut bc: i32 = -1;
        let mut len: u32 = 0;
        for id in iter {
            let b = (id >> 56) as u8;
            while bc < b as i32 {
                bc = bc + 1;
                bookmarks[bc as usize] = len;
            }
            assert!(len < u32::MAX);
            len += 1;
        }
        while bc < 256 {
            bc = bc + 1;
            bookmarks[bc as usize] = len;
        }
        Bookmarks {
            bookmarks: Box::new(bookmarks)
        }
    }

    /// Return the subslice of `xs` that contains the given id.
    pub fn range<'a, T>(&self, xs: &'a [T], id: u64) -> &'a [T] {
        let b = (id >> 56) as usize;
        let begin = self.bookmarks[b] as usize;
        let end = self.bookmarks[b + 1] as usize;
        &xs[begin..end]
    }
}

pub struct MemoryMetaIndex {
    // TODO: Use an mmappable data structure. For now this will suffice.
    artists: Vec<(ArtistId, Artist)>,
    albums: Vec<(AlbumId, Album)>,
    tracks: Vec<(TrackId, Track)>,
    artist_bookmarks: Bookmarks,
    album_bookmarks: Bookmarks,
    track_bookmarks: Bookmarks,
    strings: Vec<String>,
    filenames: Vec<String>,
}

/// Invokes `process` for all elements in the builder, in sorted order.
///
/// The arguments passed to process are `(i, id, value)`, where `i` is the
/// index of the builder. The collection iterated over is determined by
/// `project`. If the collections contain duplicates, all of them are passed to
/// `process`.
fn for_each_sorted<'a, P, I, T, F>(
    builders: &'a [BuildMetaIndex],
    project: P,
    mut process: F,
) where
  P: Fn(&'a BuildMetaIndex) -> btree_map::Iter<'a, I, T>,
  I: Clone + Eq + Ord + 'a,
  T: Clone + Eq + 'a,
  F: FnMut(usize, I, T),
{
    let mut iters: Vec<_> = builders
        .iter()
        .map(project)
        .collect();
    let mut candidates: Vec<_> = iters
        .iter_mut()
        .map(|i| i.next())
        .collect();

    // Apply the processing function to all elements from the builders in order.
    while let Some((i, _)) = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, id_val)| id_val.map(|(id, _val)| (i, id)))
            .min_by_key(|&(_, id)| id)
    {
        let mut next = iters[i].next();
        mem::swap(&mut candidates[i], &mut next);

        // Current now contains the value of `candidates[i]` before the swap,
        // which is not none, so the unwrap is safe.
        let current = next.unwrap();
        process(i, current.0.clone(), current.1.clone());
    }

    // Nothing should be left.
    for candidate in candidates {
        debug_assert!(candidate.is_none());
    }
}

impl MemoryMetaIndex {
    /// Combine builders into a memory-backed index.
    fn new(builders: &[BuildMetaIndex], issues: &mut Vec<Issue>) -> MemoryMetaIndex {
        assert!(builders.len() > 0);
        let mut artists: Vec<(ArtistId, Artist)> = Vec::new();
        let mut albums: Vec<(AlbumId, Album)> = Vec::new();
        let mut tracks: Vec<(TrackId, Track)> = Vec::new();
        let mut strings = StringDeduper::new();
        let mut filenames = Vec::new();

        for_each_sorted(builders, |b| b.tracks.iter(), |i, id, mut track| {
            // Give the track the final stringrefs, into the merged arrays.
            track.title = StringRef(
                strings.insert(builders[i].strings.get(track.title.0))
            );
            track.artist = StringRef(
                strings.insert(builders[i].strings.get(track.artist.0))
            );
            filenames.push(builders[i].filenames[track.filename.0 as usize].clone());
            track.filename = FilenameRef(filenames.len() as u32 - 1);

            if let Some(&(prev_id, ref _prev)) = tracks.last() {
                assert!(prev_id != id, "Duplicate track should not occur.");
            }

            tracks.push((id, track));
        });

        for_each_sorted(builders, |b| b.albums.iter(), |i, id, mut album| {
            album.title = StringRef(
                strings.insert(builders[i].strings.get(album.title.0))
            );

            if let Some(&(prev_id, ref prev)) = albums.last() {
                if prev_id == id {
                    if let Some(detail) = albums_different(&strings, id, prev, &album) {
                        // Report the file where the conflicting data came from.
                        let fname_index = builders[i].album_sources[&id];
                        let filename = builders[i].filenames[fname_index.0 as usize].clone();
                        let issue = detail.for_file(filename);
                        issues.push(issue);
                    }
                    return // Like `continue`, returns from the closure.
                }
            }

            albums.push((id, album));
        });

        for_each_sorted(builders, |b| b.artists.iter(), |i, id, mut artist| {
            artist.name = StringRef(
                strings.insert(builders[i].strings.get(artist.name.0))
            );
            artist.name_for_sort = StringRef(
                strings.insert(builders[i].strings.get(artist.name_for_sort.0))
            );

            if let Some(&(prev_id, ref prev)) = artists.last() {
                if prev_id == id {
                    if let Some(detail) = artists_different(&strings, id, prev, &artist) {
                        // Report the file where the conflicting data came from.
                        let fname_index = builders[i].artist_sources[&id];
                        let filename = builders[i].filenames[fname_index.0 as usize].clone();
                        let issue = detail.for_file(filename);
                        issues.push(issue);
                    }
                    return // Like `continue`, returns from the closure.
                }
            }

            artists.push((id, artist));
        });

        //println!("{} files indexed.", filenames.len());
        //println!("{} strings, {} tracks, {} albums, {} artists.",
        //         strings.strings.len(), tracks.len(), albums.len(), artists.len());

        strings.upgrade_quotes();

        MemoryMetaIndex {
            artist_bookmarks: Bookmarks::new(artists.iter().map(|p| (p.0).0)),
            album_bookmarks: Bookmarks::new(albums.iter().map(|p| (p.0).0)),
            track_bookmarks: Bookmarks::new(tracks.iter().map(|p| (p.0).0)),
            artists: artists,
            albums: albums,
            tracks: tracks,
            strings: strings.into_vec(),
            filenames: filenames,
        }
    }

    fn process<I>(paths: &Mutex<I>, builder: &mut BuildMetaIndex)
    where
        I: Iterator, <I as Iterator>::Item: AsRef<Path>
    {
        let mut progress_unreported = 0;
        loop {
            let opt_path = paths.lock().unwrap().next();
            let path = match opt_path {
                Some(p) => p,
                None => break,
            };
            let opts = claxon::FlacReaderOptions {
                metadata_only: true,
                read_picture: claxon::ReadPicture::Skip,
                read_vorbis_comment: true,
            };
            let reader = claxon::FlacReader::open_ext(path.as_ref(), opts).unwrap();
            builder.insert(path.as_ref().to_str().expect("TODO"), &reader.streaminfo(), &mut reader.tags());
            progress_unreported += 1;

            // Don't report every track individually, to avoid synchronisation
            // overhead.
            if progress_unreported == 17 {
                builder.progress.as_mut().unwrap().send(Progress::Indexed(progress_unreported)).unwrap();
                progress_unreported = 0;
            }
        }

        if progress_unreported != 0 {
            builder.progress.as_mut().unwrap().send(Progress::Indexed(progress_unreported)).unwrap();
        }

        builder.progress = None;
    }

    /// Index the given files, and store the index in the target directory.
    ///
    /// Although this streams most metadata to disk, a few parts of the index
    /// have to be kept in memory for efficient sorting, so the paths iterator
    /// should not yield *too* many elements.
    ///
    /// Reports progress to `out`, which can be `std::io::stdout().lock()`.
    pub fn from_paths<I, W>(paths: I, mut out: W) -> Result<MemoryMetaIndex>
    where I: Iterator + Send,
          W: Write,
          <I as Iterator>::Item: AsRef<Path> {
        let paths_iterator = paths.fuse();
        let mutex = Mutex::new(paths_iterator);
        let (tx_progress, rx_progress) = sync_channel(8);

        let num_threads = 24;
        let mut builders: Vec<_> = (0..num_threads)
            .map(|_| BuildMetaIndex::new(tx_progress.clone()))
            .collect();

        // Drop the original sender to ensure the channel is closed when all
        // threads are done.
        mem::drop(tx_progress);

        crossbeam::scope(|scope| {
            for builder in builders.iter_mut() {
                let mtx = &mutex;
                scope.spawn(move || MemoryMetaIndex::process(mtx, builder));
            }

            // Print issues live as indexing happens.
            let mut printed_count = false;
            let mut count = 0;
            for progress in rx_progress {
                match progress {
                    Progress::Issue(issue) => {
                        if printed_count { write!(out, "\r")?; }
                        writeln!(out, "{}\n", issue)?;
                        printed_count = false;
                    }
                    Progress::Indexed(n) => {
                        count += n;
                        if printed_count { write!(out, "\r")?; }
                        write!(out, "{} tracks indexed", count)?;
                        out.flush()?;
                        printed_count = true;
                    }
                }
            }
            if printed_count { writeln!(out, "").unwrap(); }

            // We return `Ok` here so the return type of the scope closure is
            // `io::Result`, which allows using `?` above; that's a bit nicer
            // than unwrapping everywhere. We do unwrap the result below though,
            // because `out` is likely stdout, so printing a nice error would
            // fail anyway.
            let result: io::Result<()> = Ok(());
            result
        }).unwrap();

        let mut issues = Vec::new();
        let memory_index = MemoryMetaIndex::new(&builders, &mut issues);

        // Report issues that resulted from merging.
        for issue in &issues {
            writeln!(out, "{}\n", issue).unwrap();
        }

        Ok(memory_index)
    }
}

impl MetaIndex for MemoryMetaIndex {
    #[inline]
    fn len(&self) -> usize {
        self.tracks.len()
    }

    #[inline]
    fn get_string(&self, sr: StringRef) -> &str {
        &self.strings[sr.0 as usize]
    }

    #[inline]
    fn get_filename(&self, sr: FilenameRef) -> &str {
        &self.filenames[sr.0 as usize]
    }

    #[inline]
    fn get_track(&self, id: TrackId) -> Option<&Track> {
        let slice = self.track_bookmarks.range(&self.tracks[..], id.0);
        slice
            .binary_search_by_key(&id, |pair| pair.0)
            .ok()
            // TODO: Remove bounds check.
            .map(|idx| &slice[idx].1)
    }

    #[inline]
    fn get_album(&self, id: AlbumId) -> Option<&Album> {
        let slice = self.album_bookmarks.range(&self.albums[..], id.0);
        slice
            .binary_search_by_key(&id, |pair| pair.0)
            .ok()
            // TODO: Remove bounds check.
            .map(|idx| &slice[idx].1)
    }

    #[inline]
    fn get_album_tracks(&self, id: AlbumId) -> &[(TrackId, Track)] {
        // Look for track 0 of disc 0. This is the first track of the album,
        // if it exists. Otherwise binary search would find the first track
        // after it.
        let tid = get_track_id(id, 0, 0);
        let begin = match self.tracks.binary_search_by_key(&tid, |pair| pair.0) {
            Ok(i) => i,
            Err(i) => i,
        };
        // Then do a linear scan over the tracks to find the first track that
        // does not belong to the album any more. We could do another binary
        // search to find the end instead, but a binary search would take about
        // 13 random memory accesses for 12k tracks, whereas most albums have
        // less tracks than that, and the linear scan has a very regular memory
        // access pattern.
        let end = begin + self.tracks[begin..]
            .iter()
            .position(|&(_tid, ref track)| track.album_id != id)
            .unwrap_or(self.tracks.len() - begin);

        &self.tracks[begin..end]
    }

    #[inline]
    fn get_tracks(&self) -> &[(TrackId, Track)] {
        &self.tracks
    }

    #[inline]
    fn get_albums(&self) -> &[(AlbumId, Album)] {
        &self.albums
    }

    #[inline]
    fn get_artists(&self) -> &[(ArtistId, Artist)] {
        &self.artists
    }

    #[inline]
    fn get_artist(&self, id: ArtistId) -> Option<&Artist> {
        let slice = self.artist_bookmarks.range(&self.artists[..], id.0);
        slice
            .binary_search_by_key(&id, |pair| pair.0)
            .ok()
            // TODO: Remove bounds check.
            .map(|idx| &slice[idx].1)
    }
}

#[cfg(test)]
mod tests {
    use super::{Date};
    use super::{parse_date};

    #[test]
    fn parse_date_parses_year() {
        assert_eq!(parse_date("2018"), Some(Date::new(2018, 0, 0)));
        assert_eq!(parse_date("1970"), Some(Date::new(1970, 0, 0)));
        assert_eq!(parse_date("572"), None);
        assert_eq!(parse_date("-572"), None);
        assert_eq!(parse_date("MMXVIII"), None);
        assert_eq!(parse_date("2018a"), None);
    }

    #[test]
    fn parse_date_parses_month() {
        assert_eq!(parse_date("2018-01"), Some(Date::new(2018, 1, 0)));
        assert_eq!(parse_date("2018-12"), Some(Date::new(2018, 12, 0)));
        assert_eq!(parse_date("2018-42"), None);
        assert_eq!(parse_date("2018 12"), None);
        assert_eq!(parse_date("2018-3"), None);
        assert_eq!(parse_date("2018-03a"), None);
    }

    #[test]
    fn parse_date_parses_day() {
        assert_eq!(parse_date("2018-01-01"), Some(Date::new(2018, 1, 1)));
        assert_eq!(parse_date("2018-01-31"), Some(Date::new(2018, 1, 31)));
        assert_eq!(parse_date("2018-01-32"), None);
        assert_eq!(parse_date("2018-01 01"), None);
        assert_eq!(parse_date("2018-01-1"), None);
        assert_eq!(parse_date("2018-01-01a"), None);
    }

    #[test]
    fn format_date_formats_year_only() {
        assert_eq!(format!("{}", Date::new(2018, 0, 0)), "2018");
    }

    #[test]
    fn format_date_formats_year_and_month() {
        assert_eq!(format!("{}", Date::new(2018, 1, 0)), "2018-01");
    }

    #[test]
    fn format_date_formats_year_and_month_and_day() {
        assert_eq!(format!("{}", Date::new(2018, 1, 2)), "2018-01-02");
    }
}
