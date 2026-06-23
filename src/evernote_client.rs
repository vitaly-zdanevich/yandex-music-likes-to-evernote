use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use evernote::note_store::{
    NoteFilter, NoteStoreSyncClient, NotesMetadataResultSpec, TNoteStoreSyncClient,
};
use evernote::types::{self, Data, NoteAttributes, Resource, ResourceAttributes};
use evernote::user_store::{TUserStoreSyncClient, UserStoreSyncClient};

use crate::audio::{AudioAttachment, CoverAttachment};
use reqwest::blocking::Client as ReqwestClient;
use thrift::protocol::{TBinaryInputProtocol, TBinaryOutputProtocol};
use thrift::transport::{ReadHalf, TIoChannel, WriteHalf};
use tracing::warn;

const CLIENT_NAME: &str = concat!("yandex-music-likes-to-evernote/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CREATE_NOTE_MAX_ATTEMPTS: usize = 3;
#[cfg(not(test))]
const CREATE_NOTE_RETRY_BACKOFFS: [Duration; 2] =
    [Duration::from_secs(10), Duration::from_secs(30)];
#[cfg(test)]
const CREATE_NOTE_RETRY_BACKOFFS: [Duration; 2] = [Duration::ZERO, Duration::ZERO];

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
    tag_names: Vec<String>,
    http: C,
}

impl EvernoteClient<ReqwestThriftHttpClient> {
    pub fn new(
        token: impl Into<String>,
        note_store_url: Option<String>,
        user_store_url: impl Into<String>,
        notebook_selector: Option<String>,
        tag_names: Vec<String>,
    ) -> Result<Self> {
        Ok(Self::with_http_client(
            token,
            note_store_url,
            user_store_url,
            notebook_selector,
            tag_names,
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
        tag_names: Vec<String>,
        http: C,
    ) -> Self {
        Self {
            token: token.into(),
            note_store_url,
            user_store_url: user_store_url.into(),
            resolved_note_store_url: Arc::new(Mutex::new(None)),
            notebook_selector,
            resolved_notebook_guid: Arc::new(Mutex::new(None)),
            tag_names,
            http,
        }
    }

    pub fn create_track_note(
        &self,
        title: String,
        content: String,
        source_url: String,
        cover: Option<&CoverAttachment>,
        audio: Option<&AudioAttachment>,
    ) -> Result<String> {
        let notebook_guid = self.notebook_guid()?;
        let note = types::Note {
            title: Some(title),
            content: Some(content),
            notebook_guid: notebook_guid.clone(),
            attributes: Some(NoteAttributes {
                source: Some("yandex-music-likes-to-evernote".to_string()),
                source_u_r_l: Some(source_url.clone()),
                source_application: Some(CLIENT_NAME.to_string()),
                ..NoteAttributes::default()
            }),
            tag_names: Some(self.tag_names.clone()),
            resources: note_resources(cover, audio),
            ..types::Note::default()
        };

        self.create_note_with_retries(note, notebook_guid.as_deref(), &source_url)
    }

    fn create_note_with_retries(
        &self,
        note: types::Note,
        notebook_guid: Option<&str>,
        source_url: &str,
    ) -> Result<String> {
        for attempt in 1..=CREATE_NOTE_MAX_ATTEMPTS {
            let mut client = self.note_store_client()?;
            match client.create_note(self.token.clone(), note.clone()) {
                Ok(created) => {
                    return created
                        .guid
                        .context("Evernote did not return a GUID for the created note");
                }
                Err(error)
                    if should_retry_evernote_error(&error)
                        && attempt < CREATE_NOTE_MAX_ATTEMPTS =>
                {
                    let delay = create_note_retry_delay(attempt - 1);
                    warn!(
                        attempt,
                        max_attempts = CREATE_NOTE_MAX_ATTEMPTS,
                        retry_after_seconds = delay.as_secs(),
                        error = %error,
                        "Evernote createNote failed; backing off before retry"
                    );
                    sleep(delay);
                    if let Some(guid) =
                        self.created_note_guid_by_source_url(notebook_guid, source_url)
                    {
                        warn!(
                            attempt,
                            evernote_guid = guid,
                            source_url,
                            "Evernote note exists after createNote transport failure; using existing note"
                        );
                        return Ok(guid);
                    }
                }
                Err(error) => return Err(anyhow::anyhow!("Evernote API error: {error}")),
            }
        }

        Err(anyhow::anyhow!(
            "Evernote API error: createNote retry loop ended without a result"
        ))
    }

    fn created_note_guid_by_source_url(
        &self,
        notebook_guid: Option<&str>,
        source_url: &str,
    ) -> Option<String> {
        match self.find_note_guid_by_source_url(notebook_guid, source_url) {
            Ok(guid) => guid,
            Err(error) => {
                warn!(
                    error = format!("{error:#}"),
                    source_url,
                    "failed to check Evernote for an existing note after createNote failure"
                );
                None
            }
        }
    }

    fn find_note_guid_by_source_url(
        &self,
        notebook_guid: Option<&str>,
        source_url: &str,
    ) -> Result<Option<String>> {
        let mut client = self.note_store_client()?;
        let filter = NoteFilter {
            words: Some(source_url_search_query(source_url)),
            notebook_guid: notebook_guid.map(ToOwned::to_owned),
            inactive: Some(false),
            ..NoteFilter::default()
        };
        let result_spec = NotesMetadataResultSpec {
            include_attributes: Some(true),
            include_notebook_guid: Some(true),
            ..NotesMetadataResultSpec::default()
        };
        let notes = client
            .find_notes_metadata(self.token.clone(), filter, 0, 5, result_spec)
            .map_err(|error| anyhow::anyhow!("Evernote API error: {error}"))?;

        Ok(notes
            .notes
            .into_iter()
            .find(|note| {
                note.attributes
                    .as_ref()
                    .and_then(|attributes| attributes.source_u_r_l.as_deref())
                    == Some(source_url)
            })
            .map(|note| note.guid))
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

fn should_retry_evernote_error(error: &thrift::Error) -> bool {
    matches!(
        error,
        thrift::Error::Transport(_) | thrift::Error::Protocol(_) | thrift::Error::Application(_)
    )
}

fn create_note_retry_delay(backoff_index: usize) -> Duration {
    CREATE_NOTE_RETRY_BACKOFFS
        .get(backoff_index)
        .copied()
        .unwrap_or(
            *CREATE_NOTE_RETRY_BACKOFFS
                .last()
                .expect("Evernote backoff exists"),
        )
}

fn source_url_search_query(source_url: &str) -> String {
    format!("sourceURL:{}", evernote_search_phrase(source_url))
}

fn evernote_search_phrase(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn note_resources(
    cover: Option<&CoverAttachment>,
    audio: Option<&AudioAttachment>,
) -> Option<Vec<Resource>> {
    let mut resources = Vec::new();
    if let Some(cover) = cover {
        resources.push(cover_resource(cover));
    }
    if let Some(audio) = audio {
        resources.push(audio_resource(audio));
    }

    if resources.is_empty() {
        None
    } else {
        Some(resources)
    }
}

fn cover_resource(cover: &CoverAttachment) -> Resource {
    Resource {
        data: Some(Data {
            body_hash: Some(cover.md5_raw()),
            size: Some(i32::try_from(cover.size()).unwrap_or(i32::MAX)),
            body: Some(cover.body.clone()),
        }),
        mime: Some(cover.mime.clone()),
        attributes: Some(ResourceAttributes {
            file_name: Some(cover.file_name.clone()),
            attachment: Some(false),
            ..ResourceAttributes::default()
        }),
        ..Resource::default()
    }
}

fn audio_resource(audio: &AudioAttachment) -> Resource {
    Resource {
        data: Some(Data {
            body_hash: Some(audio.md5_raw()),
            size: Some(i32::try_from(audio.size()).unwrap_or(i32::MAX)),
            body: Some(audio.body.clone()),
        }),
        mime: Some(audio.mime.clone()),
        attributes: Some(ResourceAttributes {
            file_name: Some(audio.file_name.clone()),
            attachment: Some(true),
            ..ResourceAttributes::default()
        }),
        ..Resource::default()
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
    use crate::audio::{CoverAttachment, CoverImage, TrackAudio};

    const NOTE_STORE_URL: &str = "https://www.evernote.com/shard/s1/notestore";
    const USER_STORE_URL: &str = "https://www.evernote.com/edam/user";
    const NOTEBOOK_GUID: &str = "00000000-0000-0000-0000-000000000001";
    const SOURCE_URL: &str = "https://music.yandex.com/track/123";

    fn tag_names() -> Vec<String> {
        vec!["music".to_string(), "liked tracks".to_string()]
    }

    #[derive(Clone, Default)]
    struct MockHttpClient {
        inner: Arc<Mutex<MockHttpClientInner>>,
    }

    #[derive(Default)]
    struct MockHttpClientInner {
        responses: VecDeque<Result<Vec<u8>, String>>,
        calls: Vec<MockCall>,
        create_requests: Vec<note_store::NoteStoreCreateNoteArgs>,
        find_metadata_requests: Vec<note_store::NoteStoreFindNotesMetadataArgs>,
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
                .push_back(Ok(response));
        }

        fn push_error(&self, error: impl Into<String>) {
            self.inner
                .lock()
                .expect("mock lock")
                .responses
                .push_back(Err(error.into()));
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

        fn find_metadata_requests(&self) -> Vec<note_store::NoteStoreFindNotesMetadataArgs> {
            self.inner
                .lock()
                .expect("mock lock")
                .find_metadata_requests
                .clone()
        }
    }

    impl ThriftHttpClient for MockHttpClient {
        fn post_thrift(&self, url: &str, body: Vec<u8>) -> Result<Vec<u8>, String> {
            let request = parse_mock_request(&body)?;
            let mut inner = self.inner.lock().expect("mock lock");
            inner.calls.push(MockCall {
                url: url.to_string(),
                method: request.method,
            });
            if let Some(create_request) = request.create_note {
                inner.create_requests.push(create_request);
            }
            if let Some(find_metadata_request) = request.find_notes_metadata {
                inner.find_metadata_requests.push(find_metadata_request);
            }
            inner
                .responses
                .pop_front()
                .unwrap_or_else(|| Err("No mock Evernote response queued.".to_string()))
        }
    }

    struct MockRequest {
        method: String,
        create_note: Option<note_store::NoteStoreCreateNoteArgs>,
        find_notes_metadata: Option<note_store::NoteStoreFindNotesMetadataArgs>,
    }

    fn parse_mock_request(body: &[u8]) -> Result<MockRequest, String> {
        let mut protocol = TBinaryInputProtocol::new(Cursor::new(body.to_vec()), true);
        let message = protocol
            .read_message_begin()
            .map_err(|error| error.to_string())?;
        let method = message.name.clone();
        let mut create_note = None;
        let mut find_notes_metadata = None;
        match method.as_str() {
            "createNote" => {
                create_note = Some(
                    note_store::NoteStoreCreateNoteArgs::read_from_in_protocol(&mut protocol)
                        .map_err(|error| error.to_string())?,
                );
            }
            "findNotesMetadata" => {
                find_notes_metadata = Some(
                    note_store::NoteStoreFindNotesMetadataArgs::read_from_in_protocol(
                        &mut protocol,
                    )
                    .map_err(|error| error.to_string())?,
                );
            }
            _ => {
                protocol
                    .skip(TType::Struct)
                    .map_err(|error| error.to_string())?;
            }
        };
        protocol
            .read_message_end()
            .map_err(|error| error.to_string())?;
        Ok(MockRequest {
            method,
            create_note,
            find_notes_metadata,
        })
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

    fn create_note_response_without_guid() -> Vec<u8> {
        let result = note_store::NoteStoreCreateNoteResult {
            result_value: Some(types::Note::default()),
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

    fn find_notes_metadata_response(notes: Vec<note_store::NoteMetadata>) -> Vec<u8> {
        let result = note_store::NoteStoreFindNotesMetadataResult {
            result_value: Some(note_store::NotesMetadataList {
                start_index: 0,
                total_notes: i32::try_from(notes.len()).unwrap_or(i32::MAX),
                notes,
                stopped_words: None,
                searched_words: None,
                update_count: None,
                search_context_bytes: None,
                debug_info: None,
            }),
            user_exception: None,
            system_exception: None,
            not_found_exception: None,
        };
        thrift_response("findNotesMetadata", |protocol| {
            result
                .write_to_out_protocol(protocol)
                .expect("write find notes metadata result")
        })
    }

    fn note_metadata_with_source(guid: &str, source_url: &str) -> note_store::NoteMetadata {
        note_store::NoteMetadata {
            guid: guid.to_string(),
            title: Some("Title".to_string()),
            content_length: None,
            created: None,
            updated: None,
            deleted: None,
            update_sequence_num: None,
            notebook_guid: Some(NOTEBOOK_GUID.to_string()),
            tag_guids: None,
            attributes: Some(types::NoteAttributes {
                source_u_r_l: Some(source_url.to_string()),
                ..types::NoteAttributes::default()
            }),
            largest_resource_mime: None,
            largest_resource_size: None,
        }
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
            tag_names(),
            http.clone(),
        );

        let guid = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
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
            &vec!["music".to_string(), "liked tracks".to_string()]
        );
        assert_eq!(
            requests[0]
                .note
                .attributes
                .as_ref()
                .and_then(|attributes| attributes.source_u_r_l.as_deref()),
            Some(SOURCE_URL)
        );
    }

    #[test]
    fn create_track_note_retries_transient_transport_error() {
        let http = MockHttpClient::default();
        http.push_error("temporary transport error");
        http.push_response(find_notes_metadata_response(Vec::new()));
        http.push_response(create_note_response("note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            tag_names(),
            http.clone(),
        );

        let guid = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect("retry create note");

        assert_eq!(guid, "note-guid");
        assert_eq!(
            http.calls(),
            vec![
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "findNotesMetadata".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                },
            ]
        );
        assert_eq!(http.create_requests().len(), 2);
        let find_requests = http.find_metadata_requests();
        assert_eq!(find_requests.len(), 1);
        assert_eq!(
            find_requests[0].filter.words.as_deref(),
            Some(r#"sourceURL:"https://music.yandex.com/track/123""#)
        );
    }

    #[test]
    fn create_track_note_uses_existing_note_found_after_transport_error() {
        let http = MockHttpClient::default();
        http.push_error("temporary transport error after create");
        http.push_response(find_notes_metadata_response(vec![
            note_metadata_with_source("existing-note-guid", SOURCE_URL),
        ]));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            tag_names(),
            http.clone(),
        );

        let guid = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect("existing note");

        assert_eq!(guid, "existing-note-guid");
        assert_eq!(
            http.calls(),
            vec![
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "createNote".to_string()
                },
                MockCall {
                    url: NOTE_STORE_URL.to_string(),
                    method: "findNotesMetadata".to_string()
                },
            ]
        );
        assert_eq!(http.create_requests().len(), 1);
    }

    #[test]
    fn create_track_note_attaches_audio_resource() {
        let http = MockHttpClient::default();
        http.push_response(create_note_response("note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            tag_names(),
            http.clone(),
        );
        let audio = AudioAttachment::new(
            TrackAudio {
                bytes: b"hello".to_vec(),
                codec: "flac".to_string(),
                bitrate_kbps: 1411,
                quality: "lossless".to_string(),
            },
            "Artist - Title",
        );

        client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                Some(&audio),
            )
            .expect("create note");

        let requests = http.create_requests();
        assert_eq!(requests.len(), 1);
        let resources = requests[0]
            .note
            .resources
            .as_ref()
            .expect("note should carry resources");
        assert_eq!(resources.len(), 1);
        let resource = &resources[0];
        assert_eq!(resource.mime.as_deref(), Some("audio/flac"));
        assert_eq!(
            resource
                .attributes
                .as_ref()
                .and_then(|attributes| attributes.file_name.as_deref()),
            Some("Artist - Title.flac")
        );
        let data = resource.data.as_ref().expect("resource data");
        assert_eq!(data.body.as_deref(), Some(b"hello".as_slice()));
        assert_eq!(data.size, Some(5));
        assert_eq!(
            data.body_hash.as_ref().map(|hash| hash.len()),
            Some(16),
            "body hash should be a raw MD5 digest"
        );
    }

    #[test]
    fn create_track_note_attaches_cover_resource() {
        let http = MockHttpClient::default();
        http.push_response(create_note_response("note-guid"));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            tag_names(),
            http.clone(),
        );
        let cover = CoverAttachment::new(
            CoverImage::new(b"cover".to_vec(), Some("image/png")).expect("cover image"),
        );

        client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                Some(&cover),
                None,
            )
            .expect("create note");

        let requests = http.create_requests();
        assert_eq!(requests.len(), 1);
        let resources = requests[0]
            .note
            .resources
            .as_ref()
            .expect("note should carry resources");
        assert_eq!(resources.len(), 1);
        let resource = &resources[0];
        assert_eq!(resource.mime.as_deref(), Some("image/png"));
        let attributes = resource.attributes.as_ref().expect("resource attributes");
        assert_eq!(attributes.file_name.as_deref(), Some("cover.png"));
        assert_eq!(attributes.attachment, Some(false));
        let data = resource.data.as_ref().expect("resource data");
        assert_eq!(data.body.as_deref(), Some(b"cover".as_slice()));
        assert_eq!(data.size, Some(5));
        assert_eq!(
            data.body_hash.as_ref().map(|hash| hash.len()),
            Some(16),
            "body hash should be a raw MD5 digest"
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
            tag_names(),
            http.clone(),
        );

        let first_guid = client
            .create_track_note(
                "First".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect("create first note");
        let second_guid = client
            .create_track_note(
                "Second".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
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
            tag_names(),
            http.clone(),
        );

        let first_guid = client
            .create_track_note(
                "First".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect("create first note");
        let second_guid = client
            .create_track_note(
                "Second".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
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
    fn create_track_note_reports_missing_created_note_guid() {
        let http = MockHttpClient::default();
        http.push_response(create_note_response_without_guid());
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some(NOTEBOOK_GUID.to_string()),
            tag_names(),
            http,
        );

        let error = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect_err("missing note GUID should fail");

        assert_eq!(
            error.to_string(),
            "Evernote did not return a GUID for the created note"
        );
    }

    #[test]
    fn create_track_note_reports_missing_notebook_name() {
        let http = MockHttpClient::default();
        http.push_response(list_notebooks_response(vec![types::Notebook {
            guid: Some(NOTEBOOK_GUID.to_string()),
            name: Some("Other".to_string()),
            ..types::Notebook::default()
        }]));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some("Music Inbox".to_string()),
            tag_names(),
            http,
        );

        let error = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect_err("missing notebook should fail");

        assert_eq!(
            error.to_string(),
            "Evernote notebook named 'Music Inbox' was not found"
        );
    }

    #[test]
    fn create_track_note_reports_duplicate_notebook_names() {
        let http = MockHttpClient::default();
        http.push_response(list_notebooks_response(vec![
            types::Notebook {
                guid: Some("00000000-0000-0000-0000-000000000111".to_string()),
                name: Some("Music Inbox".to_string()),
                ..types::Notebook::default()
            },
            types::Notebook {
                guid: Some("00000000-0000-0000-0000-000000000222".to_string()),
                name: Some("Music Inbox".to_string()),
                ..types::Notebook::default()
            },
        ]));
        let client = EvernoteClient::with_http_client(
            "token",
            Some(NOTE_STORE_URL.to_string()),
            USER_STORE_URL,
            Some("Music Inbox".to_string()),
            tag_names(),
            http,
        );

        let error = client
            .create_track_note(
                "Title".to_string(),
                "<en-note>Body</en-note>".to_string(),
                SOURCE_URL.to_string(),
                None,
                None,
            )
            .expect_err("duplicate notebook should fail");

        assert_eq!(
            error.to_string(),
            "Evernote returned more than one notebook named 'Music Inbox'"
        );
    }

    #[test]
    fn detects_evernote_guid_shape() {
        assert!(is_evernote_guid(NOTEBOOK_GUID));
        assert!(!is_evernote_guid("Music Inbox"));
        assert!(!is_evernote_guid("notebook-guid"));
    }
}
