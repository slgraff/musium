// Musium -- Music playback daemon with web-based library browser
// Copyright 2021 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Utilities for extracting thumbnails from flac files.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::Mutex;

use crate::database;
use crate::database::{Connection, Transaction};
use crate::database_utils;
use crate::error::{Error, Result};
use crate::prim::{AlbumId, FileId};
use crate::scan::{ScanStage, Status};
use crate::{MemoryMetaIndex, MetaIndex};

/// Tracks the process of generating a thumbnail.
struct GenThumb<'a> {
    album_id: AlbumId,
    state: GenThumbState<'a>,
}

/// The state of generating a single thumbnail.
enum GenThumbState<'a> {
    Pending {
        file_id: FileId,
        flac_filename: &'a Path,
    },
    Resizing {
        file_id: FileId,
        child: process::Child,
        out_path: PathBuf,
    },
    Compressing {
        file_id: FileId,
        child: process::Child,
        in_path: PathBuf,
    },
}

/// Return the intermediate file path where we write the resized but uncompressed thumbnail.
fn get_tmp_fname(album_id: AlbumId) -> PathBuf {
    let mut fname = std::env::temp_dir();
    fname.push(format!("musium-thumb-{}.png", album_id));
    fname
}

impl<'a> GenThumb<'a> {
    /// Create an extract-and-resize operation, if needed.
    ///
    /// If no thumbnail exists for the item yet, then this returns the task for
    /// generating the thumbnail, in the [`GenThumb::Pending`] state.
    ///
    /// TODO: In the database we should record the file id that the thumbnail
    /// was generated from, and when it no longer matches, delete the thumbnail
    /// so we can regenerate it.
    pub fn new(
        tx: &mut Transaction,
        album_id: AlbumId,
        file_id: FileId,
        flac_filename: &'a Path,
    ) -> Result<Option<GenThumb<'a>>> {
        let task = GenThumb {
            album_id: album_id,
            state: GenThumbState::Pending { flac_filename, file_id },
        };

        match database::select_thumbnail_exists(tx, album_id.0 as i64)? {
            0 => Ok(Some(task)),
            _ => Ok(None),
        }
    }

    /// From `Pending` state, read a picture, and start resizing it.
    ///
    /// Returns `None` if the input file does not contain any pictures.
    fn start_resize(
        mut self,
        album_id: AlbumId,
        file_id: FileId,
        flac_filename: &Path,
    ) -> Result<Option<GenThumb<'a>>> {
        let opts = claxon::FlacReaderOptions {
            metadata_only: true,
            read_picture: claxon::ReadPicture::CoverAsVec,
            read_vorbis_comment: false,
        };
        let reader = claxon::FlacReader::open_ext(flac_filename, opts)
            .map_err(|err| Error::from_claxon(PathBuf::from(flac_filename), err))?;

        let cover = match reader.into_pictures().pop() {
            Some(c) => c,
            None => return Ok(None),
        };

        let out_path = get_tmp_fname(album_id);

        let mut convert = Command::new("convert")
            // Read from stdin.
            .arg("-")
            // Some cover arts have an alpha channel, but we are going to encode
            // to jpeg which does not support it. First blend the image with a
            // black background, then drop the alpha channel. We also need a
            // -flatten to ensure that the subsequent distort operation uses the
            // "Edge" virtual pixel mode, rather than sampling the black
            // background. If it samples the black background, the edges of the
            // thumbnail become darker, which is especially noticeable for
            // covers with white edges, and also shows up as a "pop" in the
            // album view when the full-resolution image loads.
            .args(["-background", "black"])
            .args(["-alpha", "remove"])
            .args(["-alpha", "off"])
            .args(["-flatten"])
            // Resize in a linear color space, sRGB is not suitable for it
            // because it is nonlinear. "RGB" in ImageMagick is linear.
            .args(["-colorspace", "RGB"])
            // See also the note about -flatten above. I think Edge is the
            // default, but let's be explicit about it.
            .args(["-virtual-pixel", "Edge"])
            // Lanczos2 is a bit less sharp than Cosine, but less sharp edges
            // means that the image compresses better, and less artifacts. But
            // still, Lanczos was too blurry in my opinion.
            .args(["-filter", "Cosine"])
            // Twice the size of the thumb in the webinterface, so they appear
            // pixel-perfect on a high-DPI display, or on a mobile phone.
            .args(["-distort", "Resize", "140x140!"])
            .args(["-colorspace", "sRGB"])
            // Remove EXIF metadata, including the colour profile if there was
            // any -- we convert to sRGB anyway.
            .args(["-strip"])
            // Write lossless, we will later compress to jpeg with Guetzli,
            // which has a better compressor.
            .arg(&out_path)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| Error::CommandError("Failed to spawn ImageMagick's 'convert'.", e))?;

        {
            let stdin = convert
                .stdin
                .as_mut()
                .expect("Stdin should be there, we piped it.");
            stdin.write_all(cover.data()).unwrap();
        }

        self.state = GenThumbState::Resizing {
            file_id: file_id,
            child: convert,
            out_path: out_path,
        };

        Ok(Some(self))
    }

    /// When in `Resizing` state, wait for that to complete, and start compressing.
    fn start_compress(mut self) -> Result<GenThumb<'a>> {
        let (mut convert, file_id, out_path) = match self.state {
            GenThumbState::Resizing { file_id, child, out_path } => (child, file_id, out_path),
            _ => panic!("Can only call start_compress in Resizing state."),
        };

        convert
            .wait()
            .map_err(|e| Error::CommandError("Imagemagick's 'convert' failed.", e))?;

        let guetzli = Command::new("guetzli")
            .args(["--quality", "97"])
            // Input is the intermediate file.
            .arg(&out_path)
            // Output is stdout, but guetzli does not understand `-`.
            .stdout(Stdio::piped())
            .arg("/dev/fd/1")
            .spawn()
            .map_err(|e| Error::CommandError("Failed to spawn 'guetzli'.", e))?;

        self.state = GenThumbState::Compressing {
            file_id: file_id,
            child: guetzli,
            // Input file for this step is the output of the previous command.
            in_path: out_path,
        };

        Ok(self)
    }

    /// Take the next step that is needed to generate a thumbnail.
    ///
    /// When this returns `Some`, a process is running in the background, and we
    /// need to advance once more in the future to conclude.
    ///
    /// When this returns `None`, thumbnail generation is complete.
    fn advance(self, db: &mut Connection) -> Result<Option<GenThumb<'a>>> {
        let album_id = self.album_id;

        match self.state {
            GenThumbState::Pending {
                file_id,
                flac_filename,
            } => self.start_resize(album_id, file_id, flac_filename),
            GenThumbState::Resizing { .. } => self.start_compress().map(Some),
            GenThumbState::Compressing { mut child, file_id, in_path } => {
                child
                    .wait()
                    .map_err(|e| Error::CommandError("Guetzli failed.", e))?;

                // Delete the intermediate png file.
                std::fs::remove_file(in_path)?;

                let mut stdout = child
                    .stdout
                    .take()
                    .expect("Stdout should be there, we piped it.");
                let mut jpeg_bytes = Vec::new();
                stdout.read_to_end(&mut jpeg_bytes)?;

                {
                    let mut tx = db.begin()?;
                    database::insert_album_thumbnail(&mut tx, album_id.0 as i64, file_id.0, &jpeg_bytes[..])?;
                    tx.commit()?;
                }

                Ok(None)
            }
        }
    }
}

struct GenThumbs<'a> {
    tasks: Vec<GenThumb<'a>>,
    status: &'a mut Status,
    status_sender: &'a mut SyncSender<Status>,
}

impl<'a> GenThumbs<'a> {
    /// Take a task out of the queue, to call [`GenThumb::advance`] on.
    fn pop(&mut self) -> Option<GenThumb<'a>> {
        self.tasks.pop()
    }

    /// Handle the result of [`GenThumb::advance`].
    fn put(&mut self, result: Option<GenThumb<'a>>) {
        match result {
            Some(next_task) => self.tasks.push(next_task),
            None => {
                self.status.files_processed_thumbnails += 1;
                self.status_sender.send(*self.status).unwrap();
            }
        }
    }
}

pub fn generate_thumbnails(
    index: &MemoryMetaIndex,
    db_path: &Path,
    status: &mut Status,
    status_sender: &mut SyncSender<Status>,
) -> Result<()> {
    status.stage = ScanStage::PreProcessingThumbnails;
    status_sender.send(*status).unwrap();

    let raw_conn = database_utils::connect_readonly(db_path)?;
    let mut conn = Connection::new(&raw_conn);
    let mut tx = conn.begin()?;

    // Determine which albums need to have a new thumbnail extracted.
    let mut pending_tasks = Vec::new();
    let mut prev_album_id = AlbumId(0);
    for kv in index.get_tracks() {
        let track_id = kv.track_id;
        let album_id = track_id.album_id();
        if album_id != prev_album_id {
            let fname = index.get_filename(kv.track.filename);
            if let Some(task) = GenThumb::new(&mut tx, album_id, kv.track.file_id, fname.as_ref())? {
                pending_tasks.push(task);
                status.files_to_process_thumbnails += 1;

                if pending_tasks.len() % 32 == 0 {
                    status_sender.send(*status).unwrap();
                }
            }
            prev_album_id = album_id;
        }
    }

    tx.commit()?;
    drop(conn);
    drop(raw_conn);

    status.stage = ScanStage::GeneratingThumbnails;
    status_sender.send(*status).unwrap();

    let queue = GenThumbs {
        tasks: pending_tasks,
        status: status,
        status_sender: status_sender,
    };
    let mutex = Mutex::new(queue);
    let mutex_ref = &mutex;

    // Start 1 + `num_cpus` worker threads. All these threads will do is block
    // and wait on IO or the external process, but both `convert` and `guetzli`
    // are CPU-bound, so this should keep the CPU busy. When thumbnailing many
    // albums with a cold page cache, IO to read the thumb from the file can be
    // a factor too, so add one additional thread to ensure we can keep the CPU
    // busy. Edit: Or not, usually it's not needed.
    crossbeam::scope::<_, Result<()>>(|scope| {
        let n_threads = num_cpus::get();
        let mut threads: Vec<crossbeam::ScopedJoinHandle<Result<()>>> =
            Vec::with_capacity(n_threads);

        for i in 0..n_threads {
            let db_path_ref = db_path;
            let drain = move || {
                let raw_conn = database_utils::connect_read_write(db_path_ref)?;
                let mut conn = Connection::new(&raw_conn);

                while let Some(task) = {
                    // This has to be in a scope, otherwise the program deadlocks.
                    let mut tasks = mutex_ref.lock().unwrap();
                    tasks.pop()
                } {
                    let result = task
                        .advance(&mut conn)
                        // There is no simple way with the current version of
                        // Crossbeam to get a result out of the thread, so we
                        // just panic on error, it's what we would do elsewhere
                        // anyway if we could get the result out.
                        .expect("Thumbnail generation failed.");

                    mutex_ref.lock().unwrap().put(result);
                }

                Ok(())
            };

            let join_handle = scope
                .builder()
                .name(format!("Thumbnail generation thread {}", i))
                .spawn(drain)
                .expect("Failed to spawn OS thread.");
            threads.push(join_handle);
        }

        for join_handle in threads.drain(..) {
            // The first unwrap is on joining, that should not fail because we
            // set panic=abort. The ? then propagates any errors that might have
            // happened in the thread.
            join_handle.join()?;
        }
        Ok(())
    })
}
