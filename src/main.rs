use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::DirEntry;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use once_cell::sync::Lazy;
use regex_cursor::engines::meta::Regex;
use regex_cursor::Input;
use ropey::{Rope, RopeSlice};
use serde_json::Value;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
    document_map: Mutex<HashMap<String, Rope>>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    trigger_characters: Some(vec!['.'.to_string(), '/'.to_string()]),
                    completion_item: None,
                }),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "initialized!")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_workspace_folders(&self, _: DidChangeWorkspaceFoldersParams) {
        self.client
            .log_message(MessageType::INFO, "workspace folders changed!")
            .await;
    }

    async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
        self.client
            .log_message(MessageType::INFO, "configuration changed!")
            .await;
    }

    async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
        self.client
            .log_message(MessageType::INFO, "watched files have changed!")
            .await;
    }

    async fn execute_command(&self, _: ExecuteCommandParams) -> Result<Option<Value>> {
        self.client
            .log_message(MessageType::INFO, "command executed!")
            .await;

        Ok(None)
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file opened!")
            .await;
        let rope = Rope::from_str(&params.text_document.text);
        self.document_map
            .lock()
            .await
            .insert(params.text_document.uri.to_string(), rope);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file changed!")
            .await;
        let uri = params.text_document.uri.to_string();
        let mut document_map = self.document_map.lock().await;
        for change in params.content_changes {
            if let Some(range) = change.range {
                let rope = document_map.get_mut(&uri).unwrap();

                let start = position_to_offset(rope, range.start);
                let end = position_to_offset(rope, range.end);

                rope.remove(start..end);
                rope.insert(start, &change.text);
            } else {
                self.client
                    .log_message(MessageType::ERROR, "empty change text")
                    .await;
                assert!(change.text.is_empty());
                // document_map.insert(uri.clone(), Rope::from_str(&change.text));
            }
        }
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file saved!")
            .await;
    }

    async fn did_close(&self, _: DidCloseTextDocumentParams) {
        self.client
            .log_message(MessageType::INFO, "file closed!")
            .await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.client
            .log_message(MessageType::INFO, "completion!")
            .await;
        let uri = params.text_document_position.text_document.uri;
        let document_map = self.document_map.lock().await;
        let position = params.text_document_position.position;
        let rope = document_map.get(&uri.to_string()).unwrap();
        let line_start = rope.line_to_char(position.line as usize);
        let offset = line_start + position.character as usize;
        let line_until_cursor = rope.slice(line_start..offset);

        let Some(dir_path) = get_path_suffix(line_until_cursor, false).and_then(|matched_path| {
            let matched_path = Cow::from(matched_path);
            let path: Cow<_> = if matched_path.starts_with("file://") {
                Url::from_str(&matched_path)
                    .ok()
                    .and_then(|url| url.to_file_path().ok())?
                    .into()
            } else {
                Path::new(&*matched_path).into()
            };
            let path = expand_tilde(path);
            let parent_dir = uri.to_file_path().unwrap();
            let parent_dir = parent_dir.parent();
            let path = match parent_dir {
                Some(parent_dir) if path.is_relative() => parent_dir.join(&path),
                _ => path.into_owned(),
            };
            if matched_path.ends_with("/") {
                Some(PathBuf::from(path.as_path()))
            } else {
                path.parent().map(PathBuf::from)
            }
        }) else {
            return Ok(None);
        };

        let Ok(items) = read_dir_sorted(&dir_path, false) else {
            return Ok(None);
        };

        let items = items
            .into_iter()
            .map(|dir_entry| {
                let file_name = dir_entry.file_name();
                let file_name_str = file_name.to_string_lossy().to_string();
                let kind = dir_entry.metadata().ok().and_then(|meta| {
                    if meta.is_dir() {
                        Some(CompletionItemKind::FOLDER)
                    } else if meta.is_file() {
                        Some(CompletionItemKind::FILE)
                    } else {
                        None
                    }
                });
                CompletionItem {
                    label: file_name_str,
                    detail: None,
                    kind,
                    ..CompletionItem::default()
                }
            })
            .collect::<Vec<_>>();

        Ok(Some(CompletionResponse::Array(items)))
    }
}

fn position_to_offset(rope: &Rope, position: Position) -> usize {
    let line_start = rope.line_to_char(position.line as usize);
    line_start + position.character as usize
}

fn read_dir_sorted(path: &Path, show_hidden: bool) -> std::io::Result<Vec<DirEntry>> {
    let mut entries = std::fs::read_dir(path)?
        .flatten()
        .filter(|x| {
            !x.path().symlink_metadata().unwrap().is_symlink()
                && (show_hidden || !x.file_name().to_string_lossy().starts_with('.'))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        let a = a.path();
        let b = b.path();
        let a_name = a.file_name().unwrap().to_string_lossy().to_lowercase();
        let b_name = b.file_name().unwrap().to_string_lossy().to_lowercase();
        if a.is_dir() && b.is_dir() {
            a_name.cmp(&b_name)
        } else if a.is_dir() && !b.is_dir() {
            Ordering::Less
        } else if !a.is_dir() && b.is_dir() {
            Ordering::Greater
        } else {
            a_name.cmp(&b_name)
        }
    });
    Ok(entries)
}

fn compile_path_regex(prefix: &str, postfix: &str, match_single_file: bool) -> Regex {
    let first_component = r"(?:[\w@.\-+#$%?!,;~&]|[\^`]\s)".to_owned();
    // For all components except the first we allow an equals so that `foo=/
    // bar/baz` does not include foo. This is primarily intended for url queries
    // (where an equals is never in the first component)
    let component = format!("(?:{first_component}|=)");
    let url_prefix = r"[\w+\-.]+://??";
    let path_start = format!("(?:{first_component}+|~|{url_prefix})");
    let optional = if match_single_file {
        format!("|{path_start}")
    } else {
        String::new()
    };
    let path_regex =
        format!("{prefix}(?:{path_start}?(?:(?:/{component}+)+/?|/){optional}){postfix}");
    Regex::new(&path_regex).unwrap()
}

/// If `src` ends with a path then this function returns the part of the slice.
fn get_path_suffix(src: RopeSlice<'_>, match_single_file: bool) -> Option<RopeSlice<'_>> {
    let regex = if match_single_file {
        static REGEX: Lazy<Regex> = Lazy::new(|| compile_path_regex("", "$", true));
        &*REGEX
    } else {
        static REGEX: Lazy<Regex> = Lazy::new(|| compile_path_regex("", "$", false));
        &*REGEX
    };

    regex
        .find(Input::new(src))
        .map(|mat| src.byte_slice(mat.range()))
}

/// Expands tilde `~` into users home directory if available, otherwise returns the path
/// unchanged. The tilde will only be expanded when present as the first component of the path
/// and only slash follows it.
fn expand_tilde<'a, P>(path: P) -> Cow<'a, Path>
where
    P: Into<Cow<'a, Path>>,
{
    let path = path.into();
    let mut components = path.components();
    if let Some(Component::Normal(c)) = components.next() {
        if c == "~" {
            if let Ok(buf) = std::env::var("HOME") {
                let mut buf = PathBuf::from(buf);
                buf.push(components);
                return Cow::Owned(buf);
            }
        }
    }

    path
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());

    let (service, socket) = LspService::new(|client| Backend {
        client,
        document_map: Mutex::new(HashMap::default()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
