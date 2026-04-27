use std::{
    env::current_dir,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::{self, Child, ChildStdout, Command, Stdio},
    str::FromStr,
};

use serde_json::{json, Value};
use tower_lsp::jsonrpc::{self, Request};

// questions:
// - what is a piped stdin/stdout ?

// TODO:
// - async file io/requests
// - handle dispatching/request multiplexing

// NOTE: assumptions
// - ordering on the request ids

fn get_contents(path: impl AsRef<Path>) -> io::Result<String> {
    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    Ok(contents)
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    #[error("something happened")]
    Other,
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
struct LspServer {
    name: String,
    args: Option<Vec<String>>,
}

macro_rules! lsp {
    ($name:literal, $($arg:literal) , *) => {
        {
            let mut v = Vec::new();
            $(v.push(format!("{}", $arg)))*;
            let args = if v.is_empty() {None } else { Some(v) };
            LspServer{ name: format!("{}", $name), args }
        }
    };
}

struct LspClient {
    pub proc: Child,
    reader: BufReader<ChildStdout>,
}

impl LspClient {
    pub fn new(lsp: LspServer) -> Result<Self> {
        // launch subproc and declare init
        dbg!(&lsp);

        let mut lsp_handle = Command::new(&lsp.name)
            .args(lsp.args.as_ref().unwrap_or(&vec![]))
            .stdin(Stdio::piped()) // sending requests
            .stdout(Stdio::piped()) // receiving results
            .spawn()?;

        // crazyness
        let stdout = lsp_handle.stdout.take().unwrap();

        Ok(Self {
            proc: lsp_handle,
            reader: BufReader::new(stdout),
        })
    }

    pub fn init(&mut self, path: &str) -> Result<()> {
        // lsp init handshake
        let init = jsonrpc::Request::build("initialize")
            .id(0)
            .params(json!({
                "processId": process::id(),
                "rootPath": path,
                "capabilities": {},
            }))
            .finish();

        // fixme: replace with self.request
        self.send(&init.to_string())?;
        self.recv()?;

        // notify initalize successful
        let init_confirm_notif = jsonrpc::Request::build("initialized").finish();
        self.send(&init_confirm_notif.to_string())?;

        Ok(())
    }

    fn send(&mut self, msg: &str) -> io::Result<()> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
        write!(self.proc.stdin.as_mut().unwrap(), "{}", msg)
    }

    // the external reader steps lsp subproc stdout read state
    fn recv(&mut self) -> Result<Value> {
        let mut buf = String::new();

        let mut size: usize = 0;

        // process headers
        loop {
            buf.clear();
            self.reader.read_line(&mut buf)?;

            if buf.trim().is_empty() {
                break;
            }

            if buf.contains("Content-Length") {
                let (_, l) = buf.split_once("Content-Length: ").unwrap();
                size = l.trim().parse().unwrap();
            }
        }

        let mut response = vec![];
        response.resize(size, 0);
        self.reader.read_exact(&mut response)?;

        let resp = serde_json::from_slice::<Value>(&response)?;
        Ok(resp)
    }

    pub fn request(&mut self, req: jsonrpc::Request) -> Result<Option<jsonrpc::Response>> {
        // self.send then read from buffer, skipping notifs and asserting id's are consistent
        self.send(&req.to_string())?;

        match req.id() {
            Some(id) => {
                // WARNING: this may stall at runtime if we invoke self.recv() and attempt to readline
                // without hitting an EOF !
                while let Ok(res) = self.recv() {
                    let response = serde_json::from_value::<jsonrpc::Response>(res.clone());
                    if response.is_err() {
                        continue;
                    }

                    let parsed = response?;

                    // FIXME: we should actually store these and have a happy path for self.recv
                    if parsed.id() != id {
                        return Err(Error::Other);
                    }
                    return Ok(Some(parsed));
                }
                unreachable!();
            }
            // request was a notif
            None => Ok(None),
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.proc.kill().expect("failed to terminate lsp");
    }
}

fn main() -> Result<()> {
    let file = get_contents("python/nate.py")?;
    dbg!(&file);

    let mut client = LspClient::new(lsp!("ty", "server"))?;
    let py_dir = current_dir().map(|dir| dir.join("python")).unwrap();
    dbg!(&py_dir);

    client.init(py_dir.to_str().unwrap())?;

    // client.open("nate.py").await?.hover().await?;
    // client.close("nate.py").await?;

    // hover request
    let doc_uri = "file:///Users/nathaniel.nethercott/coding/rust/ty-lsp-subproc/nate.py";
    let did_open = jsonrpc::Request::build("textDocument/didOpen")
        .params(json!({
            "textDocument": {
                "uri": doc_uri,
                "languageId": "python",
                "version": 0,
                "text": file,
            }
        }))
        .finish();

    client.request(did_open)?;

    let hover = jsonrpc::Request::build("textDocument/references")
        .id(1)
        .params(json!({
            "textDocument": {
                "uri": doc_uri,
            },
            "position": {
                "line": 0,
                "character": 5,
            },
            "context": {
                "includeDeclaration": true,
            }
        }))
        .finish();

    let resp = client.request(hover)?;
    dbg!(resp.unwrap());

    Ok(())
}
