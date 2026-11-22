//! The real server binary on a socket of its own, and a small HTTP client for it.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::Dir;

const START_TIMEOUT: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);


pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}


impl Response {
    pub fn json(&self) -> Json {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }


    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }


    pub fn ok(self) -> Json {
        assert_eq!(self.status, 200, "{}", self.body);

        self.json()
    }
}


/// One keep-alive connection.
pub struct Client<S: Read + Write> {
    stream: BufReader<S>,
}


impl<S: Read + Write> Client<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }


    pub fn send(&mut self, method: &str, path: &str, kind: Option<&str>, body: &[u8]) {
        let kind = kind.map_or(String::new(), |kind| format!("content-type: {kind}\r\n"));
        let head = format!(
            "{method} {path} HTTP/1.1\r\nhost: test\r\n{kind}content-length: {}\r\n\r\n",
            body.len()
        );

        let stream = self.stream.get_mut();

        stream.write_all(head.as_bytes()).unwrap();

        // A request refused from its head alone (a wrong content type) is answered and the connection closed
        // without reading the body, so writing the body may fail. The answer is still there to receive.
        let _ = stream.write_all(body);
        let _ = stream.flush();
    }


    pub fn receive(&mut self) -> std::io::Result<Response> {
        let mut line = String::new();

        if self.stream.read_line(&mut line)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }

        let status = line[9..12].parse().unwrap();
        let mut headers = Vec::new();

        loop {
            line.clear();
            self.stream.read_line(&mut line)?;

            let Some((name, value)) = line.trim_end().split_once(':') else {
                break;
            };

            headers.push((name.to_string(), value.trim().to_string()));
        }

        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.parse().unwrap());

        let mut body = vec![0; length];
        self.stream.read_exact(&mut body)?;

        Ok(Response {
            status,
            headers,
            body: String::from_utf8(body).unwrap(),
        })
    }


    pub fn get(&mut self, path: &str) -> Response {
        self.send("GET", path, None, b"");
        self.receive().unwrap()
    }


    pub fn post(&mut self, path: &str, body: Json) -> Response {
        self.send(
            "POST",
            path,
            Some("application/json"),
            body.to_string().as_bytes(),
        );
        self.receive().unwrap()
    }
}


pub struct Server {
    process: Child,
    pub dir: Dir,
    pub socket: PathBuf,
}


impl Server {
    /// The small graph: ana follows ben and cleo, ben follows dan, cleo and dan follow eve; ana owns a phone.
    pub fn small() -> Self {
        let dir = Dir::new();

        dir.write(
            "people.csv",
            "key,status,age,joined\nana,active,30,2024-01-31\nben,active,41,2023-06-01\n\
             cleo,inactive,25,2025-02-03\ndan,active,52,2020-12-31\neve,active,19,2026-03-04\n",
        );
        dir.write("things.csv", "key\nphone\n");
        dir.write(
            "follows.csv",
            "a,b\nana,ben\nana,cleo\nben,dan\ncleo,eve\ndan,eve\n",
        );
        dir.write("owns.csv", "a,b\nana,phone\n");

        let schema = json!({
            "node_types": [
                {"name": "Person", "file": "people.csv", "primary_key": "key",
                 "properties": {"status": "string", "age": "integer", "joined": "date"}},
                {"name": "Thing", "file": "things.csv", "primary_key": "key", "properties": {}}
            ],
            "link_types": [
                {"name": "FOLLOWS", "from_type": "Person", "to_type": "Person",
                 "backing": {"dataset": {"file": "follows.csv", "a": "a", "b": "b"}}},
                {"name": "OWNS", "from_type": "Person", "to_type": "Thing",
                 "backing": {"dataset": {"file": "owns.csv", "a": "a", "b": "b"}}}
            ]
        });

        Self::start(dir, schema)
    }


    pub fn start(dir: Dir, schema: Json) -> Self {
        Self::start_with(dir, schema, &[])
    }


    pub fn start_with(dir: Dir, schema: Json, extra: &[&str]) -> Self {
        dir.write("schema.json", schema.to_string());

        let socket = dir.path().join("s");
        let log = fs::File::create(dir.path().join("log")).unwrap();

        let process = Command::new(binary())
            .arg("--schema")
            .arg(dir.path().join("schema.json"))
            .arg("--data")
            .arg(dir.path())
            .arg("--unix")
            .arg(&socket)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();

        let mut server = Self {
            process,
            dir,
            socket,
        };
        let started = Instant::now();

        while !server.log().contains("listening on unix") {
            assert!(server.is_alive(), "the server stopped: {}", server.log());
            assert!(started.elapsed() < START_TIMEOUT, "the server didn't start");

            std::thread::sleep(Duration::from_millis(5));
        }

        server
    }


    pub fn client(&self) -> Client<UnixStream> {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).unwrap();

        Client::new(stream)
    }


    pub fn log(&self) -> String {
        fs::read_to_string(self.dir.path().join("log")).unwrap_or_default()
    }


    /// Sends SIGTERM, as `docker stop` does, and waits up to `within` for the process to exit.
    pub fn terminate(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        Command::new("kill")
            .args(["-TERM", &self.process.id().to_string()])
            .status()
            .unwrap();

        let started = Instant::now();

        while started.elapsed() < within {
            if let Some(status) = self.process.try_wait().unwrap() {
                return Some(status);
            }

            std::thread::sleep(Duration::from_millis(10));
        }

        None
    }


    pub fn is_alive(&mut self) -> bool {
        self.process.try_wait().unwrap().is_none()
    }
}


impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}


pub fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_server"))
}


pub fn person(key: &str) -> Json {
    json!({"type": "Person", "key": key})
}
