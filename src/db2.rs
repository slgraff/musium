// This file was generated by Querybinder <TODO: version>.
// Input files:
// - src/database.sql

use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::hash_map::HashMap;

use sqlite;
use sqlite::{State::{Row, Done}, Statement};

pub type Result<T> = sqlite::Result<T>;

pub struct Connection<'a> {
    connection: &'a sqlite::Connection,
    statements: HashMap<*const u8, Statement<'a>>,
}

pub struct Transaction<'tx, 'a> {
    connection: &'a sqlite::Connection,
    statements: &'tx mut HashMap<*const u8, Statement<'a>>,
}

pub struct Iter<'i, 'a, T> {
    statement: &'i mut Statement<'a>,
    decode_row: fn(&Statement<'a>) -> Result<T>,
}

impl<'a> Connection<'a> {
    pub fn new(connection: &'a sqlite::Connection) -> Self {
        Self {
            connection,
            // TODO: We could do with_capacity here, because we know the number
            // of queries.
            statements: HashMap::new(),
        }
    }

    /// Begin a new transaction by executing the `BEGIN` statement.
    pub fn begin<'tx>(&'tx mut self) -> Result<Transaction<'tx, 'a>> {
        self.connection.execute("BEGIN;")?;
        let result = Transaction {
            connection: &self.connection,
            statements: &mut self.statements,
        };
        Ok(result)
    }
}

impl<'tx, 'a> Transaction<'tx, 'a> {
    /// Execute `COMMIT` statement.
    pub fn commit(self) -> Result<()> {
        self.connection.execute("COMMIT;")
    }

    /// Execute `ROLLBACK` statement.
    pub fn rollback(self) -> Result<()> {
        self.connection.execute("ROLLBACK;")
    }
}

impl<'i, 'a, T> Iterator for Iter<'i, 'a, T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        match self.statement.next() {
            Ok(Row) => Some((self.decode_row)(self.statement)),
            Ok(Done) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

#[derive(Debug)]
pub struct FileMetadata {
    pub filename: String,
    pub mtime: i64,
    pub streaminfo_channels: i64,
    pub streaminfo_bits_per_sample: i64,
    pub streaminfo_num_samples: Option<i64>,
    pub streaminfo_sample_rate: i64,
    pub tag_album: Option<String>,
    pub tag_albumartist: Option<String>,
    pub tag_albumartistsort: Option<String>,
    pub tag_artist: Option<String>,
    pub tag_musicbrainz_albumartistid: Option<String>,
    pub tag_musicbrainz_albumid: Option<String>,
    pub tag_discnumber: Option<String>,
    pub tag_tracknumber: Option<String>,
    pub tag_originaldate: Option<String>,
    pub tag_date: Option<String>,
    pub tag_title: Option<String>,
    pub tag_bs17704_track_loudness: Option<String>,
    pub tag_bs17704_album_loudness: Option<String>,
}

pub fn iter_file_metadata<'i, 't, 'a>(tx: &'i mut Transaction<'t, 'a>) -> Result<Iter<'i, 'a, FileMetadata>> {
    let sql = r#"
SELECT
  filename,
  mtime,
  streaminfo_channels,
  streaminfo_bits_per_sample,
  streaminfo_num_samples,
  streaminfo_sample_rate,
  tag_album,
  tag_albumartist,
  tag_albumartistsort,
  tag_artist,
  tag_musicbrainz_albumartistid,
  tag_musicbrainz_albumid,
  tag_discnumber,
  tag_tracknumber,
  tag_originaldate,
  tag_date,
  tag_title,
  tag_bs17704_track_loudness,
  tag_bs17704_album_loudness
FROM
  file_metadata
ORDER BY
  filename ASC;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    let decode_row = |statement: &Statement| Ok(FileMetadata {
        filename: statement.read(0)?,
        mtime: statement.read(1)?,
        streaminfo_channels: statement.read(2)?,
        streaminfo_bits_per_sample: statement.read(3)?,
        streaminfo_num_samples: statement.read(4)?,
        streaminfo_sample_rate: statement.read(5)?,
        tag_album: statement.read(6)?,
        tag_albumartist: statement.read(7)?,
        tag_albumartistsort: statement.read(8)?,
        tag_artist: statement.read(9)?,
        tag_musicbrainz_albumartistid: statement.read(10)?,
        tag_musicbrainz_albumid: statement.read(11)?,
        tag_discnumber: statement.read(12)?,
        tag_tracknumber: statement.read(13)?,
        tag_originaldate: statement.read(14)?,
        tag_date: statement.read(15)?,
        tag_title: statement.read(16)?,
        tag_bs17704_track_loudness: statement.read(17)?,
        tag_bs17704_album_loudness: statement.read(18)?,
    });
    let result = Iter { statement, decode_row };
    Ok(result)
}

pub fn insert_album_loudness(tx: &mut Transaction, album_id: i64, loudness: f64) -> Result<()> {
    let sql = r#"
INSERT INTO album_loudness (album_id, bs17704_loudness_lufs)
VALUES (:album_id, :loudness)
ON CONFLICT (album_id) DO UPDATE SET bs17704_loudness_lufs = :loudness;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    statement.bind(1, album_id)?;
    statement.bind(2, loudness)?;
    let result = match statement.next()? {
        Row => panic!("Query 'insert_album_loudness' unexpectedly returned a row."),
        Done => (),
    };
    Ok(result)
}

pub fn insert_track_loudness(tx: &mut Transaction, track_id: i64, loudness: f64) -> Result<()> {
    let sql = r#"
INSERT INTO track_loudness (track_id, bs17704_loudness_lufs)
VALUES (:track_id, :loudness)
ON CONFLICT (track_id) DO UPDATE SET bs17704_loudness_lufs = :loudness;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    statement.bind(1, track_id)?;
    statement.bind(2, loudness)?;
    let result = match statement.next()? {
        Row => panic!("Query 'insert_track_loudness' unexpectedly returned a row."),
        Done => (),
    };
    Ok(result)
}

pub fn insert_track_waveform(tx: &mut Transaction, track_id: i64, data: &[u8]) -> Result<()> {
    let sql = r#"
INSERT INTO waveforms (track_id, data)
VALUES (:track_id, :data)
ON CONFLICT (track_id) DO UPDATE SET data = :data;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    statement.bind(1, track_id)?;
    statement.bind(2, data)?;
    let result = match statement.next()? {
        Row => panic!("Query 'insert_track_waveform' unexpectedly returned a row."),
        Done => (),
    };
    Ok(result)
}

#[derive(Debug)]
pub struct Listen<'a> {
    pub started_at: &'a str,
    pub queue_id: i64,
    pub track_id: i64,
    pub album_id: i64,
    pub album_artist_id: i64,
    pub track_title: &'a str,
    pub track_artist: &'a str,
    pub album_title: &'a str,
    pub album_artist: &'a str,
    pub duration_seconds: i64,
    pub track_number: i64,
    pub disc_number: i64,
}

pub fn insert_listen_started(tx: &mut Transaction, listen: Listen) -> Result<i64> {
    let sql = r#"
insert into
  listens
  ( started_at
  , queue_id
  , track_id
  , album_id
  , album_artist_id
  , track_title
  , track_artist
  , album_title
  , album_artist
  , duration_seconds
  , track_number
  , disc_number
  , source
  )
values
  ( :started_at
  , :queue_id
  , :track_id
  , :album_id
  , :album_artist_id
  , :track_title
  , :track_artist
  , :album_title
  , :album_artist
  , :duration_seconds
  , :track_number
  , :disc_number
  , 'musium'
  )
returning
  id;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    statement.bind(1, listen.started_at)?;
    statement.bind(2, listen.queue_id)?;
    statement.bind(3, listen.track_id)?;
    statement.bind(4, listen.album_id)?;
    statement.bind(5, listen.album_artist_id)?;
    statement.bind(6, listen.track_title)?;
    statement.bind(7, listen.track_artist)?;
    statement.bind(8, listen.album_title)?;
    statement.bind(9, listen.album_artist)?;
    statement.bind(10, listen.duration_seconds)?;
    statement.bind(11, listen.track_number)?;
    statement.bind(12, listen.disc_number)?;
    let decode_row = |statement: &Statement| Ok(statement.read(0)?);
    let result = match statement.next()? {
        Row => decode_row(statement)?,
        Done => panic!("Query 'insert_listen_started' should return exactly one row."),
    };
    if statement.next()? != Done {
        panic!("Query 'insert_listen_started' should return exactly one row.");
    }
    Ok(result)
}

pub fn update_listen_completed(tx: &mut Transaction, listen_id: i64, queue_id: i64, track_id: i64, completed_at: &str) -> Result<()> {
    let sql = r#"
update listens
  set completed_at = :completed_at
where
  id = :listen_id
  and queue_id = :queue_id
  and track_id = :track_id;
    "#;
    let statement = match tx.statements.entry(sql.as_ptr()) {
        Occupied(entry) => entry.into_mut(),
        Vacant(vacancy) => vacancy.insert(tx.connection.prepare(sql)?),
    };
    statement.reset()?;
    statement.bind(1, completed_at)?;
    statement.bind(2, listen_id)?;
    statement.bind(3, queue_id)?;
    statement.bind(4, track_id)?;
    let result = match statement.next()? {
        Row => panic!("Query 'update_listen_completed' unexpectedly returned a row."),
        Done => (),
    };
    Ok(result)
}

// A useless main function, included only to make the example compile with
// Cargo’s default settings for examples.
fn main() {
    let raw_connection = sqlite::open(":memory:").unwrap();
    let mut connection = Connection::new(&raw_connection);

    let tx = connection.begin().unwrap();
    tx.rollback().unwrap();

    let tx = connection.begin().unwrap();
    tx.commit().unwrap();
}
