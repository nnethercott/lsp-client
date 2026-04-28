use std::io::{self, Read};
use std::{
    collections::HashMap,
    env::current_dir,
    fs::File,
    path::Path,
    process::{self, Stdio},
    sync::{
        atomic::{AtomicI64, Ordering},
        LazyLock,
    },
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tower_lsp::jsonrpc::{self};

static REQUEST_ID: AtomicI64 = AtomicI64::new(0);
static RESPONSE_REGISTRY: LazyLock<Mutex<HashMap<jsonrpc::Id, jsonrpc::Response>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

    #[allow(dead_code)]
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
    stdout: Mutex<BufReader<ChildStdout>>,
    stdin: Mutex<ChildStdin>,
}

impl LspClient {
    pub fn new(lsp: LspServer) -> Result<Self> {
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
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
        })
    }

    pub async fn init(&mut self, path: &str) -> Result<()> {
        self.request_or_notify(
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

        // ok; initialized
        self.request_or_notify(jsonrpc::Request::build("initialized").finish())
            .await?;

        Ok(())
    }

    async fn send(&self, msg: &str) -> io::Result<usize> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", msg.len(), msg);
        self.stdin.lock().await.write(msg.as_bytes()).await
    }

    // the external reader steps lsp subproc stdout read state
    async fn recv(&self) -> Result<Value> {
        let mut stdout_guard = self.stdout.lock().await;
        let mut buf = String::new();

        let mut size: usize = 0;

        // process headers
        loop {
            buf.clear();
            stdout_guard.read_line(&mut buf).await?;
            if buf.trim().is_empty() {
                break;
            }
            if buf.contains("Content-Length") {
                let (_, l) = buf.split_once("Content-Length: ").unwrap();
                size = l.trim().parse().unwrap();
            }
        }

        let mut response = vec![0; size];
        stdout_guard.read_exact(&mut response).await?;

        drop(stdout_guard);

        let resp = serde_json::from_slice::<Value>(&response)?;
        Ok(resp)
    }

    pub async fn request_or_notify(
        &self,
        req: jsonrpc::Request,
    ) -> Result<Option<jsonrpc::Response>> {
        self.send(&req.to_string()).await?;

        // FIXME: here we're aiming for an atomic "read stdout and update global registry"
        // but its bad since a) worst case everything is inserted in RAM into `RESPONSE_REGISTRY` 
        // and b) holding the registry mutex guard is blocking.
        // I don't, however, want to decouple the stdout read from the send op since i feel like
        // there's a clever way to do this
        match req.id() {
            Some(id) => loop {
                let mut registry_guard = RESPONSE_REGISTRY.lock().await;

                if let Some(value) = registry_guard.remove(id) {
                    return Ok(Some(value));
                }

                let res = self.recv().await?;
                let response = serde_json::from_value::<jsonrpc::Response>(res);
                if response.is_err() {
                    continue;
                }

                let parsed = response?;

                if parsed.id() != id {
                    registry_guard.insert(parsed.id().clone(), parsed);
                    continue;
                }
                return Ok(Some(parsed));
            },
            // request was a notification
            None => Ok(None),
        }
    }
}

// impl Drop for LspClient {
//     fn drop(&mut self) {
//         // FIXME: do i need to box leak this somehow?
//         let drop_fut = async move || self.proc.kill().await.expect("failed to terminate lsp");
//         tokio::spawn(drop_fut());
//     }
// }

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut client = LspClient::new(lsp!("ty", "server"))?;

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

    client
        .request_or_notify(open_doc(&file_uri, contents))
        .await?;

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

    let _res = tokio::join!(
        client.request_or_notify(ref_factory(&file_uri, 0, 5)),
        client.request_or_notify(ref_factory(&file_uri, 0, 5)),
    );

    client.proc.kill().await.expect("failed to kill subproc");

    Ok(())
}
