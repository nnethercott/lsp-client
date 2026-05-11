use std::{
    env::current_dir,
    fs::File,
    io::{self, Read},
    path::Path,
};

use pspsps::{Driver, Error, LspClient, LspCmd, Result, next_id, serve};
use serde_json::json;
use tower_lsp::jsonrpc;

macro_rules! lsp {
    ($name:literal, $($arg:literal) , *) => {
        {
            let mut v = Vec::new();
            $(v.push(format!("{}", $arg)))*;
            let args = if v.is_empty() {None } else { Some(v) };
            LspCmd{ name: format!("{}", $name), args }
        }
    };
}

pub fn get_server_client() -> Result<(Driver, LspClient)> {
    let (tx, rx) = tokio::sync::mpsc::channel(8);

    let driver = Driver::new(lsp!("ty", "server"), rx)?;
    let client = LspClient::new(tx);

    Ok((driver, client))
}

fn get_contents(path: impl AsRef<Path>) -> io::Result<String> {
    let mut contents = String::new();
    File::open(path)?.read_to_string(&mut contents)?;
    Ok(contents)
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
