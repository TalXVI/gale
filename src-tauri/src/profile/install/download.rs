use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

use eyre::{Context, Result, eyre};
use futures_util::StreamExt;
use http::header;
use reqwest::{StatusCode, header::HeaderValue};
use reqwest_middleware::ClientWithMiddleware;
use tauri::AppHandle;
use tracing::{debug, warn};

use crate::state::ManagerExt;

use super::{
    InstallError, InstallEvent, InstallOptions, InstallResult, InstallTask, ModInstall,
    check_cancel, emit,
};

const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("package download exceeds its {0} byte limit")]
struct DownloadLimit(u64);

/// Downloads the archive of the given mod into memory, retrying (and resuming) if
/// the connection is interrupted.
pub(super) async fn download(
    install: &ModInstall,
    cancel: &AtomicBool,
    options: &InstallOptions,
    app: &AppHandle,
) -> InstallResult<Vec<u8>> {
    emit(
        InstallEvent::set_task(&install.ident, InstallTask::Download),
        app,
    );

    let url = install.id.backend.download_url(&install.ident);

    debug!(
        ident = %install.ident,
        size = install.file_size,
        url = %url,
        "downloading mod"
    );

    let progress = |bytes| emit(InstallEvent::AddProgress { mods: 0, bytes }, app);
    let check = || check_cancel(cancel, options);

    fetch(app.http(), &url, install.file_size, &progress, &check).await
}

/// Where a download's bytes go. A resumed download appends at [`len`].
///
/// [`len`]: DownloadSink::len
pub(crate) trait DownloadSink: Send {
    fn len(&self) -> u64;
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
    /// Discards everything received so far.
    fn clear(&mut self) -> Result<()>;
}

impl DownloadSink for Vec<u8> {
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.try_reserve(bytes.len())
            .context("failed to allocate package download buffer")?;
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        Vec::clear(self);
        Ok(())
    }
}

/// Streams a download to a file instead of holding it in memory.
pub(crate) struct FileSink {
    file: std::fs::File,
    len: u64,
}

impl FileSink {
    /// Takes ownership of an empty file opened for writing.
    pub(crate) fn new(file: std::fs::File) -> Self {
        Self { file, len: 0 }
    }

    /// The file holding the complete download, rewound to its start.
    pub(crate) fn into_file(mut self) -> Result<std::fs::File> {
        use std::io::{Seek, SeekFrom};
        self.file
            .seek(SeekFrom::Start(0))
            .context("failed to rewind package download")?;
        Ok(self.file)
    }
}

impl DownloadSink for FileSink {
    fn len(&self) -> u64 {
        self.len
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        self.file
            .write_all(bytes)
            .context("failed to write package download")?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        use std::io::{Seek, SeekFrom};
        self.file
            .set_len(0)
            .and_then(|()| self.file.seek(SeekFrom::Start(0)).map(drop))
            .context("failed to discard partial package download")?;
        self.len = 0;
        Ok(())
    }
}

/// Downloads the file at `url` into memory, retrying (and resuming) if the
/// connection is interrupted.
///
/// `progress` receives the change in downloaded bytes, which is negative when a
/// partial download has to be discarded. `check_cancel` is polled while data
/// arrives and stops the download when it returns an error.
pub(crate) async fn fetch(
    client: &ClientWithMiddleware,
    url: &str,
    size_hint: u64,
    progress: &(dyn Fn(i64) + Sync),
    check_cancel: &(dyn Fn() -> InstallResult<()> + Sync),
) -> InstallResult<Vec<u8>> {
    let mut response = Vec::new();
    fetch_into(
        &mut response,
        client,
        url,
        size_hint,
        progress,
        check_cancel,
    )
    .await?;
    Ok(response)
}

/// Like [`fetch`], but writes the download to `sink`, which must start empty.
pub(crate) async fn fetch_into(
    sink: &mut dyn DownloadSink,
    client: &ClientWithMiddleware,
    url: &str,
    size_hint: u64,
    progress: &(dyn Fn(i64) + Sync),
    check_cancel: &(dyn Fn() -> InstallResult<()> + Sync),
) -> InstallResult<()> {
    // how many times in a row a download may fail without making any progress
    // before we give up
    const MAX_RETRIES: usize = 5;
    // hard limit on the total number of attempts, so that we don't retry forever
    // if the connection keeps getting dropped right after receiving some data
    const MAX_ATTEMPTS: usize = 20;
    const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    let limit = if size_hint == 0 {
        MAX_DOWNLOAD_BYTES
    } else {
        size_hint.min(MAX_DOWNLOAD_BYTES)
    };
    if size_hint > MAX_DOWNLOAD_BYTES {
        return Err(eyre!(DownloadLimit(MAX_DOWNLOAD_BYTES)).into());
    }
    let mut retries = 0;
    let mut attempts = 0;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let downloaded = sink.len();
        attempts += 1;

        match try_download(sink, url, client, limit, progress, check_cancel).await {
            Ok(()) => break Ok(()),
            Err(InstallError::Cancelled) => return Err(InstallError::Cancelled),
            Err(InstallError::Error(err)) => {
                // errors like a 404 won't resolve themselves, so retrying is pointless
                if is_fatal_download_error(&err) {
                    break Err(InstallError::Error(err));
                }

                // as long as the download keeps making progress, the connection itself
                // is working, so keep retrying from where it left off
                if sink.len() > downloaded {
                    debug!(
                        downloaded = sink.len(),
                        total = size_hint,
                        "download interrupted, will resume where it left off"
                    );

                    retries = 0;
                    backoff = INITIAL_BACKOFF;
                } else {
                    retries += 1;
                }

                if retries >= MAX_RETRIES || attempts >= MAX_ATTEMPTS {
                    break Err(InstallError::Error(err.wrap_err("max retries exceeded")));
                }

                warn!(
                    attempt = attempts,
                    downloaded = sink.len(),
                    total = size_hint,
                    err = ?err,
                    url = %url,
                    backoff = ?backoff,
                    "download failed, retrying"
                );

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Downloads the file at `url` into `sink`, resuming from the end of `sink` if it
/// already contains (partial) data.
///
/// Errors out if the server starts sending at an unexpected offset, or if the
/// connection is closed before the whole file was received, so that the caller can
/// retry the download where it left off.
async fn try_download(
    buf: &mut dyn DownloadSink,
    url: &str,
    client: &ClientWithMiddleware,
    limit: u64,
    progress: &(dyn Fn(i64) + Sync),
    check_cancel: &(dyn Fn() -> InstallResult<()> + Sync),
) -> InstallResult<()> {
    const UPDATE_DELAY: Duration = Duration::from_millis(100);

    let (response, expected_len) = match send_download_request(buf, url, client, progress).await? {
        Some((res, len)) => (res, len),
        None => return Ok(()), // download already complete
    };

    if expected_len.is_some_and(|len| len > limit) {
        return Err(eyre!(DownloadLimit(limit)).into());
    }

    let mut stream = response.bytes_stream();
    let mut last_update = Instant::now();
    let mut last_size_update = buf.len() as i64;

    while let Some(item) = stream.next().await {
        let item = item.context("failed to read chunk from stream")?;
        check_cancel()?;
        if item.len() as u64 > limit - buf.len() {
            return Err(eyre!(DownloadLimit(limit)).into());
        }
        buf.append(&item)?;

        if last_update.elapsed() >= UPDATE_DELAY {
            last_update = Instant::now();
            progress(buf.len() as i64 - last_size_update);
            last_size_update = buf.len() as i64;

            check_cancel()?;
        }
    }

    progress(buf.len() as i64 - last_size_update);

    // the connection may have been closed cleanly before all data was sent
    if let Some(expected_len) = expected_len
        && buf.len() != expected_len
    {
        return Err(eyre!(
            "incomplete download: received {} out of {} bytes",
            buf.len(),
            expected_len
        )
        .into());
    }

    Ok(())
}

async fn send_download_request(
    buf: &mut dyn DownloadSink,
    url: &str,
    client: &ClientWithMiddleware,
    progress: &(dyn Fn(i64) + Sync),
) -> Result<Option<(reqwest::Response, Option<u64>)>, InstallError> {
    let resumed_from = buf.len();

    let mut request = client
        .get(url)
        // ask for an uncompressed body, since we can't resume an archive from the
        // byte offsets of a compressed response
        .header(header::ACCEPT_ENCODING, "identity");

    if resumed_from > 0 {
        request = request.header(header::RANGE, format!("bytes={resumed_from}-"));
    }

    let response = request.send().await.context("failed to send request")?;
    let status = response.status();
    let version = response.version();

    debug!(url = %url, resumed_from, %status, ?version, "download requested");

    let response = match status {
        StatusCode::PARTIAL_CONTENT if resumed_from > 0 => response,
        // the server has no more data to give us, so the download is already complete
        StatusCode::RANGE_NOT_SATISFIABLE if resumed_from > 0 => {
            let total = response
                .headers()
                .get(header::CONTENT_RANGE)
                .map(parse_content_range)
                .and_then(|range| range.total);

            if total == Some(resumed_from) {
                debug!(url = %url, downloaded = resumed_from, "download already complete");
                return Ok(None);
            }

            discard_partial_download(buf, progress)?;
            return Err(eyre!("server rejected range request at offset {resumed_from}").into());
        }
        // the server ignored the range request, so the whole file must be downloaded again
        StatusCode::OK if resumed_from > 0 => {
            warn!(
                url = %url,
                downloaded = resumed_from,
                "server ignored range request, restarting download"
            );

            discard_partial_download(buf, progress)?;
            response
        }
        _ => response.error_for_status().context("request failed")?,
    };

    let expected_start = buf.len();

    let content_range = response
        .headers()
        .get(header::CONTENT_RANGE)
        .map(parse_content_range);

    if let Some(start) = content_range.and_then(|range| range.start)
        && start != expected_start
    {
        discard_partial_download(buf, progress)?;
        return Err(eyre!("unexpected download offset {start}, expected {expected_start}").into());
    }

    let expected_len = content_range.and_then(|range| range.total).or_else(|| {
        // if the body isn't compressed, the content length must match the data we receive
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            // body is compressed and the content range wasn't provided; we can't know the total size of the request
            None
        } else {
            response
                .content_length()
                .map(|len| len.saturating_add(expected_start))
        }
    });

    Ok(Some((response, expected_len)))
}

/// The parts of a [`CONTENT_RANGE`] header that we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRange {
    start: Option<u64>,
    total: Option<u64>,
}

/// Parses a `Content-Range` header like `bytes 0-1023/2048` or `bytes */2048`.
fn parse_content_range(value: &HeaderValue) -> ContentRange {
    let value = value.to_str().unwrap_or_default();
    let value = value.strip_prefix("bytes ").unwrap_or(value);
    let (bounds, total) = value.split_once('/').unwrap_or((value, ""));

    let start = bounds.split_once('-').map_or(bounds, |(start, _)| start);

    ContentRange {
        start: start.trim().parse().ok(),
        total: total.trim().parse().ok(),
    }
}

/// Clears a partially downloaded file, undoing its contribution to the install progress.
fn discard_partial_download(
    buf: &mut dyn DownloadSink,
    progress: &(dyn Fn(i64) + Sync),
) -> Result<()> {
    if buf.len() == 0 {
        return Ok(());
    }

    progress(-(buf.len() as i64));

    buf.clear()
}

/// Whether an error from [`try_download`] is permanent, in which case retrying is pointless.
fn is_fatal_download_error(err: &eyre::Report) -> bool {
    if err.is::<DownloadLimit>() || err.is::<std::collections::TryReserveError>() {
        return true;
    }
    let Some(status) = err
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
    else {
        return false;
    };

    // timeouts and rate limits are worth retrying, unlike (for example) 404s
    status.is_client_error()
        && status != StatusCode::REQUEST_TIMEOUT
        && status != StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn security_stops_a_chunked_download_at_its_byte_budget() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mod.zip", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let count = socket.read(&mut request).await.unwrap();
            assert!(
                std::str::from_utf8(&request[..count])
                    .unwrap()
                    .starts_with("GET /mod.zip HTTP/1.1")
            );
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n").await.unwrap();
        });
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            fetch(&client, &url, 8, &|_| {}, &|| Ok(())),
        )
        .await
        .unwrap();
        task.await.unwrap();
        match result {
            Err(InstallError::Error(error)) => assert!(
                error.is::<DownloadLimit>(),
                "unexpected download error: {error:#}"
            ),
            _ => panic!("accepted a body larger than the advertised package"),
        }
    }

    #[tokio::test]
    async fn a_file_download_restarted_by_the_server_keeps_only_the_new_body() {
        use std::io::Read;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mod.zip", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            // The first response closes mid-body. The resumed request is then
            // answered from the start, as a server that ignores ranges does.
            for response in [
                &b"HTTP/1.1 200 OK
Content-Length: 10
Connection: close

01234"[..],
                b"HTTP/1.1 200 OK
Content-Length: 10
Connection: close

ABCDEFGHIJ",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                let count = socket.read(&mut request).await.unwrap();
                requests.push(String::from_utf8_lossy(&request[..count]).to_lowercase());
                socket.write_all(response).await.unwrap();
            }
            requests
        });
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let mut sink = FileSink::new(tempfile::tempfile().unwrap());
        tokio::time::timeout(
            Duration::from_secs(10),
            fetch_into(&mut sink, &client, &url, 0, &|_| {}, &|| Ok(())),
        )
        .await
        .unwrap()
        .unwrap();
        let requests = task.await.unwrap();
        assert!(requests[1].contains("range: bytes=5-"), "{}", requests[1]);
        let mut downloaded = String::new();
        sink.into_file()
            .unwrap()
            .read_to_string(&mut downloaded)
            .unwrap();
        assert_eq!(downloaded, "ABCDEFGHIJ");
    }

    fn parse(value: &str) -> ContentRange {
        parse_content_range(&HeaderValue::from_str(value).unwrap())
    }

    #[test]
    fn start_and_total() {
        assert_eq!(
            parse("bytes 0-1023/2048"),
            ContentRange {
                start: Some(0),
                total: Some(2048)
            }
        );
    }

    #[test]
    fn resumed_download() {
        // what the thunderstore CDN responds with when resuming a download
        assert_eq!(
            parse("bytes 1024-1055/778380268"),
            ContentRange {
                start: Some(1024),
                total: Some(778380268)
            }
        );
    }

    #[test]
    fn unsatisfiable_range() {
        // a 416 response only tells us the total size of the file
        assert_eq!(
            parse("bytes */2048"),
            ContentRange {
                start: None,
                total: Some(2048)
            }
        );
    }

    #[test]
    fn unknown_total() {
        assert_eq!(
            parse("bytes 1000-1999/*"),
            ContentRange {
                start: Some(1000),
                total: None
            }
        );
    }

    #[test]
    fn lenient_formatting() {
        let expected = ContentRange {
            start: Some(0),
            total: Some(2048),
        };

        // missing "bytes " prefix
        assert_eq!(parse("0-1023/2048"), expected);

        // stray whitespace around the offsets
        assert_eq!(parse("bytes 0-1023 / 2048"), expected);
    }

    #[test]
    fn invalid() {
        let empty = ContentRange {
            start: None,
            total: None,
        };

        assert_eq!(parse(""), empty);
        assert_eq!(parse("bytes"), empty);
        assert_eq!(parse("bytes abc-def/ghi"), empty);
    }
}
