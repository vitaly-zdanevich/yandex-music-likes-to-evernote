use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use evernote::note_store::{NoteStoreSyncClient, TNoteStoreSyncClient};
use evernote::types::{self, NoteAttributes};
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
    note_store_url: String,
    notebook_guid: Option<String>,
    http: C,
}

impl EvernoteClient<ReqwestThriftHttpClient> {
    pub fn new(
        token: impl Into<String>,
        note_store_url: impl Into<String>,
        notebook_guid: Option<String>,
    ) -> Result<Self> {
        Ok(Self::with_http_client(
            token,
            note_store_url,
            notebook_guid,
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
        note_store_url: impl Into<String>,
        notebook_guid: Option<String>,
        http: C,
    ) -> Self {
        Self {
            token: token.into(),
            note_store_url: note_store_url.into(),
            notebook_guid,
            http,
        }
    }

    pub fn create_track_note(&self, title: String, content: String) -> Result<String> {
        let mut client = self.note_store_client()?;
        let note = types::Note {
            title: Some(title),
            content: Some(content),
            notebook_guid: self.notebook_guid.clone(),
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
        let channel = ThriftHttpChannel::new(self.note_store_url.clone(), self.http.clone());
        let (read, write) = channel
            .split()
            .map_err(|error| anyhow::anyhow!("failed to initialize Evernote transport: {error}"))?;
        Ok(NoteStoreSyncClient::new(
            TBinaryInputProtocol::new(read, true),
            TBinaryOutputProtocol::new(write, true),
        ))
    }
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

    use evernote::note_store;
    use pretty_assertions::assert_eq;
    use thrift::protocol::{TInputProtocol, TMessageIdentifier, TMessageType, TOutputProtocol};

    use super::*;

    const NOTE_STORE_URL: &str = "https://www.evernote.com/shard/s1/notestore";

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

    #[test]
    fn create_track_note_sends_create_note_request() {
        let http = MockHttpClient::default();
        http.push_response(create_note_response("note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            NOTE_STORE_URL,
            Some("notebook-guid".to_string()),
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
            Some("notebook-guid")
        );
        assert_eq!(
            requests[0].note.tag_names.as_ref().unwrap(),
            &vec!["yandex-music".to_string()]
        );
    }
}
