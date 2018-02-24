extern crate futures;
extern crate hyper;
extern crate metaindex;
extern crate serde_json;
extern crate walkdir;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Read;
use std::path::PathBuf;
use std::process;
use std::rc::Rc;

use futures::future::Future;
use hyper::header::{ContentLength, ContentType};
use hyper::mime;
use hyper::server::{Http, Request, Response, Service};
use hyper::{Get, StatusCode};
use metaindex::{AlbumId, MetaIndex, MemoryMetaIndex, TrackId};

struct MetaServer {
    index: MemoryMetaIndex,
}

type BoxFuture = Box<Future<Item=Response, Error=hyper::Error>>;

impl MetaServer {
    fn new(index: MemoryMetaIndex) -> MetaServer {
        MetaServer {
            index: index,
        }
    }

    fn handle_not_found(&self) -> BoxFuture {
        let not_found = "Not Found";
        let response = Response::new()
            .with_status(StatusCode::NotFound)
            .with_header(ContentLength(not_found.len() as u64))
            .with_body(not_found);
        Box::new(futures::future::ok(response))
    }

    fn handle_bad_request(&self, reason: &'static str) -> BoxFuture {
        let response = Response::new()
            .with_status(StatusCode::BadRequest)
            .with_header(ContentLength(reason.len() as u64))
            .with_body(reason);
        Box::new(futures::future::ok(response))
    }

    fn handle_error(&self, reason: &'static str) -> BoxFuture {
        let response = Response::new()
            .with_status(StatusCode::InternalServerError)
            .with_header(ContentLength(reason.len() as u64))
            .with_body(reason);
        Box::new(futures::future::ok(response))
    }

    fn handle_track(&self, _request: &Request, path: &str) -> BoxFuture {
        // Track urls are of the form `/track/f7c153f2b16dc101.flac`.
        if !path.ends_with(".flac") {
            return self.handle_bad_request("Expected a path ending in .flac.")
        }

        let id_part = &path[..path.len() - ".flac".len()];
        let track_id = match TrackId::parse(id_part) {
            Some(tid) => tid,
            None => return self.handle_bad_request("Invalid track id."),
        };

        let track = match self.index.get_track(track_id) {
            Some(t) => t,
            None => return self.handle_not_found(),
        };

        let fname = self.index.get_filename(track.filename);

        // TODO: Rather than reading the file into memory in userspace, use
        // sendfile. Hyper seems to be over-engineered for my use case, just
        // writing to a TCP socket would be simpler.
        let mut file = match fs::File::open(fname) {
            Ok(f) => f,
            Err(_) => return self.handle_error("Failed to open file."),
        };
        let len_hint = file.metadata().map(|m| m.len()).unwrap_or(4096);
        let mut body = Vec::with_capacity(len_hint as usize);
        if let Err(_) = file.read_to_end(&mut body) {
            return self.handle_error("Failed to read file.")
        }

        // TODO: Handle requests with Range header.
        let audio_flac = "audio/flac".parse::<mime::Mime>().unwrap();
        let response = Response::new()
            .with_header(ContentType(audio_flac))
            .with_header(ContentLength(body.len() as u64))
            .with_body(body);
        Box::new(futures::future::ok(response))
    }

    fn handle_album(&self, _request: &Request, id: &str) -> BoxFuture {
        let album_id = match AlbumId::parse(id) {
            Some(aid) => aid,
            None => return self.handle_bad_request("Invalid album id."),
        };

        let album = match self.index.get_album(album_id) {
            Some(a) => a,
            None => return self.handle_not_found(),
        };

        let buffer = Vec::new();
        let mut w = io::Cursor::new(buffer);
        self.index.write_album_json(&mut w, album_id, album).unwrap();
        let response = Response::new()
            .with_header(ContentType::json())
            .with_body(w.into_inner());
        Box::new(futures::future::ok(response))
    }

    fn handle_albums(&self, _request: &Request) -> BoxFuture {
        let buffer = Vec::new();
        let mut w = io::Cursor::new(buffer);
        self.index.write_albums_json(&mut w).unwrap();
        let response = Response::new()
            .with_header(ContentType::json())
            .with_body(w.into_inner());
        Box::new(futures::future::ok(response))
    }

    fn handle_artist(&self, _request: &Request, _id: &str) -> BoxFuture {
        let response = Response::new().with_body("Artist");
        Box::new(futures::future::ok(response))
    }

    fn handle_search(&self, _request: &Request) -> BoxFuture {
        let response = Response::new().with_body("Search");
        Box::new(futures::future::ok(response))
    }
}

impl Service for MetaServer {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = BoxFuture;

    fn call(&self, request: Request) -> Self::Future {
        println!("Request: {:?}", request);

        let mut parts = request
            .uri()
            .path()
            .splitn(3, '/')
            .filter(|x| x.len() > 0);

        let p0 = parts.next();
        let p1 = parts.next();

        // A very basic router. See also docs/api.md for an overview.
        match (request.method(), p0, p1) {
            (&Get, Some("track"),  Some(t)) => self.handle_track(&request, t),
            (&Get, Some("album"),  Some(a)) => self.handle_album(&request, a),
            (&Get, Some("albums"), None)    => self.handle_albums(&request),
            (&Get, Some("artist"), Some(a)) => self.handle_artist(&request, a),
            (&Get, Some("search"), None)    => self.handle_search(&request),
            (&Get, _, _) => self.handle_not_found(),
            _ => self.handle_bad_request("Expected a GET request."),
        }
    }
}

fn main() {
    if env::args().len() < 2 {
        println!("usage: index /path/to/music/library");
        process::exit(1);
    }

    let dir = env::args().nth(1).unwrap();
    let wd = walkdir::WalkDir::new(&dir)
        .follow_links(true)
        .max_open(128);

    let flac_ext = OsStr::new("flac");

    let paths: Vec<_> = wd
        .into_iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file())
        .map(|e| PathBuf::from(e.path()))
        .filter(|p| p.extension() == Some(flac_ext))
        .collect();

    let index = {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        metaindex::MemoryMetaIndex::from_paths(paths.iter(), &mut lock)
            .expect("Failed to build index.")
    };
    println!("Index has {} tracks.", index.len());
    println!("Indexing complete, starting server on port 8233.");
    let service = Rc::new(MetaServer::new(index));
    let addr = ([0, 0, 0, 0], 8233).into();
    let server = Http::new().bind(&addr, move || Ok(service.clone())).unwrap();
    server.run().unwrap();
}
