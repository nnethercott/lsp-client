#![feature(unsafe_cell_access)]

use std::cell::UnsafeCell;
use std::io::{self, Read};
use std::{
    collections::HashMap,
    env::current_dir,
    fs::File,
    path::Path,
    process::{self, Stdio},
    sync::atomic::{AtomicI64, Ordering},
};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use tower_lsp::jsonrpc::{self};

static REQUEST_ID: AtomicI64 = AtomicI64::new(0);

fn next_id() -> i64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

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

    #[error("failed to send/recv event")]
    Tokio,

    #[allow(dead_code)]
    #[error("something happened")]
    Other,
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
enum Action {
    Request {
        req: jsonrpc::Request,
        tx: oneshot::Sender<jsonrpc::Response>,
    },
    Notify {
        req: jsonrpc::Request,
    },
}

impl Action {
    pub fn contents(&self) -> &jsonrpc::Request {
        match self {
            Action::Request { req, .. } => req,
            Action::Notify { req } => req,
        }
    }
}

#[derive(Debug)]
struct Lsp {
    name: String,
    args: Option<Vec<String>>,
}

macro_rules! lsp {
    ($name:literal, $($arg:literal) , *) => {
        {
            let mut v = Vec::new();
            $(v.push(format!("{}", $arg)))*;
            let args = if v.is_empty() {None } else { Some(v) };
            Lsp{ name: format!("{}", $name), args }
        }
    };
}

struct LspServer {
    proc: Child,
    stdout: BufReader<ChildStdout>,
    stdin: ChildStdin,
}

impl LspServer {
    pub fn new(lsp: Lsp) -> Result<Self> {
        dbg!(&lsp);

        let mut lsp_handle = Command::new(&lsp.name)
            .args(lsp.args.as_ref().unwrap_or(&vec![]))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = lsp_handle.stdout.take().unwrap();
        let stdin = lsp_handle.stdin.take().unwrap();

        Ok(Self {
            proc: lsp_handle,
            stdin: stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn read_one(&mut self) -> Result<Value> {
        let mut buf = String::new();
        let mut size: usize = 0;

        // process headers
        loop {
            buf.clear();
            self.stdout.read_line(&mut buf).await?;
            if buf.trim().is_empty() {
                break;
            }
            if buf.contains("Content-Length") {
                let (_, l) = buf.split_once("Content-Length: ").unwrap();
                size = l.trim().parse().unwrap();
            }
        }

        let mut response = vec![0; size];
        self.stdout.read_exact(&mut response).await?;

        let resp = serde_json::from_slice::<Value>(&response)?;
        Ok(resp)
    }

    async fn write_one(&mut self, msg: &str) -> io::Result<usize> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
        self.stdin.write(msg.as_bytes()).await
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        let _ = self.proc.start_kill();
    }
}

struct Driver {
    server: LspServer,
    incoming: Receiver<Action>,
    sendback_channels: HashMap<jsonrpc::Id, oneshot::Sender<jsonrpc::Response>>,
}

impl Driver {
    pub fn new(lsp: Lsp, rx: Receiver<Action>) -> Result<Self> {
        Ok(Self {
            server: LspServer::new(lsp)?,
            incoming: rx,
            sendback_channels: HashMap::new(),
        })
    }

    async fn enqueue(&mut self) -> Result<()> {
        while let Some(act) = self.incoming.recv().await {
            let msg = serde_json::to_string(act.contents())?;

            let _ = match act {
                Action::Request { req, tx } => {
                    let id = req.id().unwrap();
                    let _ = self.sendback_channels.insert(id.clone(), tx);

                    self.server.write_one(&msg).await.map_err(|e| {
                        let _ = self.sendback_channels.remove(id);
                        e
                    })?
                }
                Action::Notify { .. } => self.server.write_one(&msg).await?,
            };
        }
        Ok(())
    }

    async fn dequeue(&mut self) -> Result<()> {
        loop {
            let res = self.server.read_one().await?;
            let response = serde_json::from_value::<jsonrpc::Response>(res);

            // read past notifications
            if response.is_err() {
                continue;
            }

            let parsed = response?;

            let tx = self
                .sendback_channels
                .remove(parsed.id())
                .ok_or(Error::Other)?;

            tx.send(parsed).map_err(|_| Error::Tokio)?;
        }
    }
}

struct LspClient(Sender<Action>);

impl LspClient {
    pub async fn init(&self, path: &str) -> Result<()> {
        self.request(
            jsonrpc::Request::build("initialize")
                .id(next_id())
                .params(json!({
                    "processId": process::id(),
                    "rootPath": path,
                    "capabilities": {},
                }))
                .finish(),
        )
        .await?;

        self.notify(jsonrpc::Request::build("initialized").finish())
            .await?;

        Ok(())
    }

    pub async fn request(&self, req: jsonrpc::Request) -> Result<jsonrpc::Response> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let act = Action::Request { req, tx };
        self.0.send(act).await.map_err(|_| Error::Tokio)?;
        rx.await.map_err(|_| Error::Tokio)
    }

    pub async fn notify(&self, req: jsonrpc::Request) -> Result<()> {
        let act = Action::Notify { req };
        self.0.send(act).await.map_err(|_| Error::Tokio)?;
        Ok(())
    }
}

async fn serve(instance: &mut Driver) -> Result<()> {
    // SAFETY: LspServer::read_one steps the child processes stdout read state, but we only
    // do this single threaded. c.f. for LspServer::write_one.
    // We should actually split ownership of the read/write fds into different structs, but flemme
    let unsafe_instance = UnsafeCell::new(instance);

    let write_fut = unsafe { unsafe_instance.as_mut_unchecked().enqueue() };
    let read_fut = unsafe { unsafe_instance.as_mut_unchecked().dequeue() };

    tokio::select! {
       res = write_fut => res,
       res = read_fut => res,
    }
}

fn get_server_client() -> Result<(Driver, LspClient)> {
    let (tx, rx) = tokio::sync::mpsc::channel(8);

    let driver = Driver::new(lsp!("ty", "server"), rx)?;
    let client = LspClient(tx);

    Ok((driver, client))
}

// the main application logic
async fn silly(client: LspClient) -> Result<()> {
    let py_dir = current_dir().map(|dir| dir.join("python")).unwrap();
    client.init(py_dir.to_str().unwrap()).await?;

    let open_doc = |doc_uri: &str, contents: String| {
        jsonrpc::Request::build("textDocument/didOpen")
            .params(json!({
                "textDocument": {
                    "uri": doc_uri,
                    "languageId": "python",
                    "version": 0,
                    "text": contents,
                }
            }))
            .finish()
    };

    let file = py_dir.join("nate.py");
    let file_uri = format!("file://{}", file.to_str().unwrap());
    let contents = get_contents("python/nate.py")?;

    client.notify(open_doc(&file_uri, contents)).await?;

    let ref_factory = |doc_uri: &str, line: usize, char: usize| {
        jsonrpc::Request::build("textDocument/references")
            .id(next_id())
            .params(json!({
                "textDocument": {
                    "uri": doc_uri,
                },
                "position": {
                    "line": line,
                    "character": char,
                },
                "context": {
                    "includeDeclaration": true,
                }
            }))
            .finish()
    };

    let res = tokio::join!(
        client.request(ref_factory(&file_uri, 0, 5)),
        client.request(ref_factory(&file_uri, 0, 5)),
    );
    let _ = dbg!(res);
    Ok::<(), Error>(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let (mut lsp, client) = get_server_client()?;

    let mut actor = async move || {
        serve(&mut lsp).await?;
        Ok::<(), Error>(())
    };

    tokio::select! {
        _ = silly(client) => (),
        _ = actor() => (),
    }

    Ok(())
}
