use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use evernote::note_store::{NoteStoreSyncClient, TNoteStoreSyncClient};
use evernote::types::{self, NoteAttributes};
use evernote::user_store::{TUserStoreSyncClient, UserStoreSyncClient};
use reqwest::blocking::Client as ReqwestClient;
use thrift::protocol::{TBinaryInputProtocol, TBinaryOutputProtocol};
use thrift::transport::{ReadHalf, TIoChannel, WriteHalf};

const CLIENT_NAME: &str = "yandex-music-likes-to-evernote/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

type InputProtocol<C> = TBinaryInputProtocol<ReadHalf<ThriftHttpChannel<C>>>;
type OutputProtocol<C> = TBinaryOutputProtocol<WriteHalf<ThriftHttpChannel<C>>>;

pub trait ThriftHttpClient: Clone + Send + Sync + 'static {
    fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String>;
}

#[derive(Clone)]
pub struct ReqwestThriftHttpClient {
    client: ReqwestClient,
}

impl ReqwestThriftHttpClient {
    pub fn new() -> Result<Self> {
        let client = ReqwestClient::builder()
            .user_agent(CLIENT_NAME)
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(2)
            .build()
            .context("failed to build Evernote HTTP client")?;
        Ok(Self { client })
    }
}

impl ThriftHttpClient for ReqwestThriftHttpClient {
    fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-thrift")
            .body(body)
            .send()
            .map_err(|error| format!("Evernote request failed: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Evernote returned an HTTP error: {error}"))?;

        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("failed to read Evernote response: {error}"))
    }
}

#[derive(Clone)]
pub struct EvernoteClient<C = ReqwestThriftHttpClient>
where
    C: ThriftHttpClient,
{
    token: String,
    note_store_url: Option<String>,
    user_store_url: String,
    resolved_note_store_url: Arc<Mutex<Option<String>>>,
    notebook_selector: Option<String>,
    resolved_notebook_guid: Arc<Mutex<Option<String>>>,
    http: C,
}

impl EvernoteClient<ReqwestThriftHttpClient> {
    pub fn new(
        token: impl Into<String>,
        note_store_url: Option<String>,
        user_store_url: impl Into<String>,
        notebook_selector: Option<String>,
    ) -> Result<Self> {
        Ok(Self::with_http_client(
            token,
            note_store_url,
            user_store_url,
            notebook_selector,
            ReqwestThriftHttpClient::new()?,
        ))
    }
}

impl<C> EvernoteClient<C>
where
    C: ThriftHttpClient,
{
    pub fn with_http_client(
        token: impl Into<String>,
        note_store_url: Option<String>,
        user_store_url: impl Into<String>,
        notebook_selector: Option<String>,
        http: C,
    ) -> Self {
        Self {
            token: token.into(),
            note_store_url,
            user_store_url: user_store_url.into(),
            resolved_note_store_url: Arc::new(Mutex::new(None)),
            notebook_selector,
            resolved_notebook_guid: Arc::new(Mutex::new(None)),
            http,
        }
    }

    pub fn create_track_note(&self, title: String, content: String) -> Result<String> {
        let notebook_guid = self.notebook_guid()?;
        let mut client = self.note_store_client()?;
        let note = types::Note {
            title: Some(title),
            content: Some(content),
            notebook_guid,
            attributes: Some(NoteAttributes {
                source: Some("yandex-music-likes-to-evernote".to_string()),
                source_application: Some(CLIENT_NAME.to_string()),
                ..NoteAttributes::default()
            }),
            tag_names: Some(vec!["yandex-music".to_string()]),
            ..types::Note::default()
        };

        let created = client
            .create_note(self.token.clone(), note)
            .map_err(|error| anyhow::anyhow!("Evernote API error: {error}"))?;

        created
            .guid
            .context("Evernote did not return a GUID for the created note")
    }

    fn note_store_client(
        &self,
    ) -> Result<NoteStoreSyncClient<InputProtocol<C>, OutputProtocol<C>>> {
        let note_store_url = self.note_store_url()?;
        let channel = ThriftHttpChannel::new(note_store_url, self.http.clone());
        let (read, write) = channel
            .split()
            .map_err(|error| anyhow::anyhow!("failed to initialize Evernote transport: {error}"))?;
        Ok(NoteStoreSyncClient::new(
            TBinaryInputProtocol::new(read, true),
            TBinaryOutputProtocol::new(write, true),
        ))
    }

    fn user_store_client(
        &self,
    ) -> Result<UserStoreSyncClient<InputProtocol<C>, OutputProtocol<C>>> {
        let channel = ThriftHttpChannel::new(self.user_store_url.clone(), self.http.clone());
        let (read, write) = channel
            .split()
            .map_err(|error| anyhow::anyhow!("failed to initialize Evernote transport: {error}"))?;
        Ok(UserStoreSyncClient::new(
            TBinaryInputProtocol::new(read, true),
            TBinaryOutputProtocol::new(write, true),
        ))
    }

    fn note_store_url(&self) -> Result<String> {
        if let Some(note_store_url) = &self.note_store_url {
            return Ok(note_store_url.clone());
        }

        if let Some(note_store_url) = self
            .resolved_note_store_url
            .lock()
            .map_err(|_| anyhow::anyhow!("Evernote NoteStore URL cache is poisoned"))?
            .clone()
        {
            return Ok(note_store_url);
        }

        let note_store_url = self.fetch_note_store_url()?;
        *self
            .resolved_note_store_url
            .lock()
            .map_err(|_| anyhow::anyhow!("Evernote NoteStore URL cache is poisoned"))? =
            Some(note_store_url.clone());
        Ok(note_store_url)
    }

    fn fetch_note_store_url(&self) -> Result<String> {
        let mut client = self.user_store_client()?;
        let urls = client
            .get_user_urls(self.token.clone())
            .map_err(|error| anyhow::anyhow!("Evernote UserStore API error: {error}"))?;
        urls.note_store_url
            .context("Evernote UserStore did not return a NoteStore URL")
    }

    fn notebook_guid(&self) -> Result<Option<String>> {
        let Some(selector) = &self.notebook_selector else {
            return Ok(None);
        };

        if is_evernote_guid(selector) {
            return Ok(Some(selector.clone()));
        }

        if let Some(guid) = self
            .resolved_notebook_guid
            .lock()
            .map_err(|_| anyhow::anyhow!("Evernote notebook GUID cache is poisoned"))?
            .clone()
        {
            return Ok(Some(guid));
        }

        let guid = self.fetch_notebook_guid_by_name(selector)?;
        *self
            .resolved_notebook_guid
            .lock()
            .map_err(|_| anyhow::anyhow!("Evernote notebook GUID cache is poisoned"))? =
            Some(guid.clone());
        Ok(Some(guid))
    }

    fn fetch_notebook_guid_by_name(&self, name: &str) -> Result<String> {
        let mut client = self.note_store_client()?;
        let notebooks = client
            .list_notebooks(self.token.clone())
            .map_err(|error| anyhow::anyhow!("Evernote API error: {error}"))?;
        let mut matches = notebooks
            .into_iter()
            .filter(|notebook| notebook.name.as_deref() == Some(name));
        let notebook = matches
            .next()
            .with_context(|| format!("Evernote notebook named '{name}' was not found"))?;
        if matches.next().is_some() {
            return Err(anyhow::anyhow!(
                "Evernote returned more than one notebook named '{name}'"
            ));
        }

        notebook
            .guid
            .with_context(|| format!("Evernote notebook named '{name}' did not include a GUID"))
    }
}

fn is_evernote_guid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }

    bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    })
}

#[derive(Clone)]
struct ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    endpoint: String,
    http: C,
    state: Arc<Mutex<ThriftHttpState>>,
}

#[derive(Default)]
struct ThriftHttpState {
    read_bytes: Vec<u8>,
    read_pos: usize,
    write_bytes: Vec<u8>,
}

impl<C> ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn new(endpoint: String, http: C) -> Self {
        Self {
            endpoint,
            http,
            state: Arc::new(Mutex::new(ThriftHttpState::default())),
        }
    }
}

impl<C> TIoChannel for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn split(self) -> thrift::Result<(ReadHalf<Self>, WriteHalf<Self>)>
    where
        Self: Sized,
    {
        Ok((ReadHalf::new(self.clone()), WriteHalf::new(self)))
    }
}

impl<C> Read for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
        let remaining = state.read_bytes.len().saturating_sub(state.read_pos);
        let read_len = remaining.min(buf.len());
        if read_len == 0 {
            return Ok(0);
        }

        let start = state.read_pos;
        let end = start + read_len;
        buf[..read_len].copy_from_slice(&state.read_bytes[start..end]);
        state.read_pos = end;
        Ok(read_len)
    }
}

impl<C> Write for ThriftHttpChannel<C>
where
    C: ThriftHttpClient,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
        state.write_bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let request_body = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
            std::mem::take(&mut state.write_bytes)
        };

        let response_body = self
            .http
            .post_thrift(&self.endpoint, request_body)
            .map_err(io::Error::other)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("Evernote transport state is poisoned"))?;
        state.read_bytes = response_body;
        state.read_pos = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;

    use evernote::{note_store, user_store};
    use pretty_assertions::assert_eq;
    use thrift::protocol::{
        TFieldIdentifier, TInputProtocol, TMessageIdentifier, TMessageType, TOutputProtocol,
        TSerializable, TStructIdentifier, TType,
    };

    use super::*;

    const NOTE_STORE_URL: &str = "https://www.evernote.com/shard/s1/notestore";
    const USER_STORE_URL: &str = "https://www.evernote.com/edam/user";
    const NOTEBOOK_GUID: &str = "00000000-0000-0000-0000-000000000001";

    #[derive(Clone, Default)]
    struct MockHttpClient {
        inner: Arc<Mutex<MockHttpClientInner>>,
    }

    #[derive(Default)]
    struct MockHttpClientInner {
        responses: VecDeque<Vec<u8>>,
        calls: Vec<MockCall>,
        create_requests: Vec<note_store::NoteStoreCreateNoteArgs>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct MockCall {
        url: String,
        method: String,
    }

    impl MockHttpClient {
        fn push_response(&self, response: Vec<u8>) {
            self.inner
                .lock()
                .expect("mock lock")
                .responses
                .push_back(response);
        }

        fn calls(&self) -> Vec<MockCall> {
            self.inner.lock().expect("mock lock").calls.clone()
        }

        fn create_requests(&self) -> Vec<note_store::NoteStoreCreateNoteArgs> {
            self.inner
                .lock()
                .expect("mock lock")
                .create_requests
                .clone()
        }
    }

    impl ThriftHttpClient for MockHttpClient {
        fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
            let (method, create_request) = parse_mock_request(&body)?;
            let mut inner = self.inner.lock().expect("mock lock");
            inner.calls.push(MockCall {
                url: url.to_string(),
                method,
            });
            if let Some(create_request) = create_request {
                inner.create_requests.push(create_request);
            }
            inner
                .responses
                .pop_front()
                .ok_or_else(|| "No mock Evernote response queued.".to_string())
        }
    }

    fn parse_mock_request(
        body: &[u8],
    ) -> Result<(String, Option<note_store::NoteStoreCreateNoteArgs>), String> {
        let mut protocol = TBinaryInputProtocol::new(Cursor::new(body.to_vec()), true);
        let message = protocol
            .read_message_begin()
            .map_err(|error| error.to_string())?;
        let method = message.name.clone();
        let create_request = if method == "createNote" {
            Some(
                note_store::NoteStoreCreateNoteArgs::read_from_in_protocol(&mut protocol)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            protocol
                .skip(TType::Struct)
                .map_err(|error| error.to_string())?;
            None
        };
        protocol
            .read_message_end()
            .map_err(|error| error.to_string())?;
        Ok((method, create_request))
    }

    fn thrift_response(
        method: &str,
        write_result: impl FnOnce(&mut dyn TOutputProtocol),
    ) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut protocol = TBinaryOutputProtocol::new(&mut buffer, true);
            protocol
                .write_message_begin(&TMessageIdentifier::new(method, TMessageType::Reply, 1))
                .expect("write message begin");
            write_result(&mut protocol);
            protocol.write_message_end().expect("write message end");
            protocol.flush().expect("flush response");
        }
        buffer
    }

    fn create_note_response(guid: &str) -> Vec<u8> {
        let result = note_store::NoteStoreCreateNoteResult {
            result_value: Some(types::Note {
                guid: Some(guid.to_string()),
                ..types::Note::default()
            }),
            user_exception: None,
            system_exception: None,
            not_found_exception: None,
        };
        thrift_response("createNote", |protocol| {
            result
                .write_to_out_protocol(protocol)
                .expect("write create note result")
        })
    }

    fn user_urls_response(note_store_url: &str) -> Vec<u8> {
        let urls = user_store::UserUrls {
            note_store_url: Some(note_store_url.to_string()),
            ..user_store::UserUrls::default()
        };
        thrift_response("getUserUrls", |protocol| {
            protocol
                .write_struct_begin(&TStructIdentifier::new("UserStoreGetUserUrlsResult"))
                .expect("write getUserUrls result begin");
            protocol
                .write_field_begin(&TFieldIdentifier::new("result_value", TType::Struct, 0))
                .expect("write getUserUrls result field begin");
            urls.write_to_out_protocol(protocol)
                .expect("write UserUrls result");
            protocol
                .write_field_end()
                .expect("write getUserUrls result field end");
            protocol
                .write_field_stop()
                .expect("write getUserUrls result field stop");
            protocol
                .write_struct_end()
                .expect("write getUserUrls result end");
        })
    }

    fn list_notebooks_response(notebooks: Vec<types::Notebook>) -> Vec<u8> {
        let result = note_store::NoteStoreListNotebooksResult {
            result_value: Some(notebooks),
            user_exception: None,
            system_exception: None,
        };
        thrift_response("listNotebooks", |protocol| {
            result
                .write_to_out_protocol(protocol)
                .expect("write list notebooks result")
        })
    }

    #[test]
    fn create_track_note_sends_create_note_request() {
        let http = MockHttpClient::default();
        http.push_response(create_note_response("note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            http.clone(),
        );

        let guid = client
            .create_track_note("Title".to_string(), "<en-note>Body</en-note>".to_string())
            .expect("create note");

        assert_eq!(guid, "note-guid");
        assert_eq!(
            http.calls(),
            vec![MockCall {
                url: NOTE_STORE_URL.to_string(),
                method: "createNote".to_string()
            }]
        );
        let requests = http.create_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].authentication_token, "token");
        assert_eq!(requests[0].note.title.as_deref(), Some("Title"));
        assert_eq!(
            requests[0].note.notebook_guid.as_deref(),
            Some(NOTEBOOK_GUID)
        );
        assert_eq!(
            requests[0].note.tag_names.as_ref().unwrap(),
            &vec!["yandex-music".to_string()]
        );
    }

    #[test]
    fn create_track_note_discovers_note_store_url_when_missing() {
        let http = MockHttpClient::default();
        http.push_response(user_urls_response(NOTE_STORE_URL));
        http.push_response(create_note_response("first-note-guid"));
        http.push_response(create_note_response("second-note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            None,
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            http.clone(),
        );

        let first_guid = client
            .create_track_note("First".to_string(), "<en-note>Body</en-note>".to_string())
            .expect("create first note");
        let second_guid = client
            .create_track_note("Second".to_string(), "<en-note>Body</en-note>".to_string())
            .expect("create second note");

        assert_eq!(first_guid, "first-note-guid");
        assert_eq!(second_guid, "second-note-guid");
        assert_eq!(
            http.calls(),
            vec![
                MockCall {
                    url: USER_STORE_URL.to_string(),
                    method: "getUserUrls".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                }
            ]
        );
    }

    #[test]
    fn create_track_note_resolves_notebook_name_once() {
        let http = MockHttpClient::default();
        http.push_response(list_notebooks_response(vec![
            types::Notebook {
                guid: Some("00000000-0000-0000-0000-000000000999".to_string()),
                name: Some("Other".to_string()),
                ..types::Notebook::default()
            },
            types::Notebook {
                guid: Some(NOTEBOOK_GUID.to_string()),
                name: Some("Music Inbox".to_string()),
                ..types::Notebook::default()
            },
        ]));
        http.push_response(create_note_response("first-note-guid"));
        http.push_response(create_note_response("second-note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some("Music Inbox".to_string()),
            http.clone(),
        );

        let first_guid = client
            .create_track_note("First".to_string(), "<en-note>Body</en-note>".to_string())
            .expect("create first note");
        let second_guid = client
            .create_track_note("Second".to_string(), "<en-note>Body</en-note>".to_string())
            .expect("create second note");

        assert_eq!(first_guid, "first-note-guid");
        assert_eq!(second_guid, "second-note-guid");
        assert_eq!(
            http.calls(),
            vec![
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "listNotebooks".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                }
            ]
        );
        let requests = http.create_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].note.notebook_guid.as_deref(),
            Some(NOTEBOOK_GUID)
        );
        assert_eq!(
            requests[1].note.notebook_guid.as_deref(),
            Some(NOTEBOOK_GUID)
        );
    }

    #[test]
    fn detects_evernote_guid_shape() {
        assert!(is_evernote_guid(NOTEBOOK_GUID));
        assert!(!is_evernote_guid("Music Inbox"));
        assert!(!is_evernote_guid("notebook-guid"));
    }
}
