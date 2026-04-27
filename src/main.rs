use std::{
    env::current_dir,
    fs::File,
    io::{self, Read},
    path::Path,
    process::{self, Stdio},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
};
use tower_lsp::jsonrpc;

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
        dbg!(&lsp);

        let mut lsp_handle = Command::new(&lsp.name)
            .args(lsp.args.as_ref().unwrap_or(&vec![]))
            .stdin(Stdio::piped()) // sending requests
            .stdout(Stdio::piped()) // receiving results
            .spawn()?;

        let stdout = lsp_handle.stdout.take().unwrap();

        Ok(Self {
            proc: lsp_handle,
            reader: BufReader::new(stdout),
        })
    }

    pub async fn init(&mut self, path: &str) -> Result<()> {
        // lsp init handshake
        let init = jsonrpc::Request::build("initialize")
            .id(0)
            .params(json!({
                "processId": process::id(),
                "rootPath": path,
                "capabilities": {},
            }))
            .finish();

        self.request_or_notify(init).await?;

        // notify initalize successful
        let init_confirm_notif = jsonrpc::Request::build("initialized").finish();
        self.request_or_notify(init_confirm_notif).await?;

        Ok(())
    }

    async fn send(&mut self, msg: &str) -> io::Result<usize> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
        self.proc
            .stdin
            .as_mut()
            .unwrap()
            .write(msg.as_bytes())
            .await
    }

    // the external reader steps lsp subproc stdout read state
    async fn recv(&mut self) -> Result<Value> {
        let mut buf = String::new();

        let mut size: usize = 0;

        // process headers
        loop {
            buf.clear();
            self.reader.read_line(&mut buf).await?;

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
        self.reader.read_exact(&mut response).await?;

        let resp = serde_json::from_slice::<Value>(&response)?;
        Ok(resp)
    }

    pub async fn request_or_notify(
        &mut self,
        req: jsonrpc::Request,
    ) -> Result<Option<jsonrpc::Response>> {
        // self.send then read from buffer, skipping notifs and asserting id's are consistent
        self.send(&req.to_string()).await?;

        match req.id() {
            Some(id) => {
                // WARNING: this may stall at runtime if we invoke self.recv() and attempt to readline
                // without hitting an EOF !
                while let Ok(res) = self.recv().await {
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

// impl Drop for LspClient {
//     fn drop(&mut self) {
//         // FIXME: do i need to leak this somehow?
//         let drop_fut = async move || self.proc.kill().await.expect("failed to terminate lsp");
//         tokio::spawn(drop_fut());
//     }
// }

#[tokio::main]
async fn main() -> Result<()> {
    let file = get_contents("python/nate.py")?;
    dbg!(&file);

    let mut client = LspClient::new(lsp!("ty", "server"))?;
    let py_dir = current_dir().map(|dir| dir.join("python")).unwrap();
    dbg!(&py_dir);

    client.init(py_dir.to_str().unwrap()).await?;

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

    client.request_or_notify(did_open).await?;

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

    let resp = client.request_or_notify(hover).await?;
    dbg!(resp.unwrap());

    client.proc.kill().await.expect("failed to kill subproc");

    Ok(())
}
