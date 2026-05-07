#![feature(unsafe_cell_access)]

use std::cell::UnsafeCell;
use std::io::{self};
use std::{
    collections::HashMap,
    process::{self, Stdio},
    sync::atomic::{AtomicI64, Ordering},
};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use tower_lsp::jsonrpc::{self};

static REQUEST_ID: AtomicI64 = AtomicI64::new(0);

pub fn next_id() -> i64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Action {
    Request {
        req: jsonrpc::Request,
        tx: oneshot::Sender<jsonrpc::Response>,
    },
    Notify {
        req: jsonrpc::Request,
        // no tx needed
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
pub struct LspCmd {
    pub name: String,
    pub args: Option<Vec<String>>,
}

struct LspServer {
    proc: Child,
    stdout: BufReader<ChildStdout>,
    stdin: ChildStdin,
}

impl LspServer {
    pub fn new(lsp: LspCmd) -> Result<Self> {
        dbg!(&lsp);

        let mut lsp_handle = Command::new(&lsp.name)
            .args(lsp.args.as_ref().unwrap_or(&vec![]))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdout = lsp_handle
            .stdout
            .take()
            .expect("failed to take child stdout");
        let stdin = lsp_handle.stdin.take().expect("failed to take child stdin");

        Ok(Self {
            proc: lsp_handle,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn read_message_bytes(&mut self) -> Result<Vec<u8>> {
        let mut buf = String::new();
        let mut size: usize = 0;

        // process headers
        loop {
            buf.clear();
            self.stdout.read_line(&mut buf).await?;
            if buf.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
            if buf.contains("Content-Length") {
                let (_, l) = buf.split_once("Content-Length: ").unwrap();
                size = l.trim().parse().unwrap();
            }
        }

        let mut bytes = vec![0; size];
        self.stdout.read_exact(&mut bytes).await?;
        Ok(bytes)
    }

    async fn read_message(&mut self) -> Result<Value> {
        let response = self.read_message_bytes().await?;
        Ok(serde_json::from_slice::<Value>(&response)?)
    }

    async fn write_one(&mut self, msg: &str) -> io::Result<()> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
        self.stdin.write_all(msg.as_bytes()).await?;
        let _ = self.stdin.flush().await;
        Ok(())
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        let _ = self.proc.start_kill();
    }
}

pub struct Driver {
    server: LspServer,
    incoming: Receiver<Action>,
    sendback_channels: HashMap<jsonrpc::Id, oneshot::Sender<jsonrpc::Response>>,
}

impl Driver {
    pub fn new(lsp: LspCmd, rx: Receiver<Action>) -> Result<Self> {
        Ok(Self {
            server: LspServer::new(lsp)?,
            incoming: rx,
            sendback_channels: HashMap::new(),
        })
    }

    async fn enqueue(&mut self) -> Result<()> {
        while let Some(act) = self.incoming.recv().await {
            let msg = serde_json::to_string(act.contents())?;

            match act {
                Action::Request { req, tx } => {
                    let id = req.id().unwrap();
                    let _ = self.sendback_channels.insert(id.clone(), tx);

                    self.server.write_one(&msg).await.inspect_err(|e| {
                        let _ = self.sendback_channels.remove(id);
                    })?
                }
                Action::Notify { .. } => self.server.write_one(&msg).await?,
            };
        }
        Ok(())
    }

    async fn dequeue(&mut self) -> Result<()> {
        loop {
            let res = self.server.read_message().await?;
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

pub struct LspClient(Sender<Action>);

impl LspClient {
    pub fn new(tx: Sender<Action>) -> Self {
        Self(tx)
    }

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

pub async fn serve(instance: &mut Driver) -> Result<()> {
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
