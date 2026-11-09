//! Downloads a release file to disk, bounded in size and time.
use std::path::Path;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::drivers::DriverResult;

use super::error;

/// The largest archive accepted. Releases are about 40 MB.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// A whole download.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How much of a download has arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    /// From `Content-Length`, when the server sent it.
    pub total: Option<u64>,
}

impl Progress {
    /// Whole percent, when the size is known.
    pub fn percent(self) -> Option<u8> {
        let total = self.total.filter(|total| *total > 0)?;
        let percent = self.received.saturating_mul(100) / total;
        Some(u8::try_from(percent.min(100)).unwrap_or(100))
    }
}

/// Downloads `url` into `dest` (created or truncated), refusing more than
/// `cap` bytes, and reports progress at most every 250 ms or 1 %. The
/// caller bounds the whole time and cancels by dropping the future. Never
/// logs the address: a redirect carries a signed token in its query.
pub async fn download(
    client: &reqwest_genai::Client,
    url: &str,
    dest: &Path,
    cap: u64,
    progress: &watch::Sender<Progress>,
) -> DriverResult<u64> {
    let mut response = client
        .get(url)
        .header(reqwest_genai::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|e| error::from_reqwest(&e.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(error::from_status(status.as_u16()));
    }
    let total = response.content_length();
    if total.is_some_and(|total| total > cap) {
        return Err(error::too_large());
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| error::from_io(&e))?;
    let mut received: u64 = 0;
    let mut reported = (Instant::now(), 0u64);
    progress.send_replace(Progress { received, total });
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| error::from_reqwest(&e.without_url()))?
    {
        received = received.saturating_add(chunk.len() as u64);
        if received > cap {
            return Err(error::too_large());
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| error::from_io(&e))?;
        let step = total.map_or(u64::MAX, |total| total / 100);
        if reported.0.elapsed() >= Duration::from_millis(250) || received - reported.1 >= step {
            progress.send_replace(Progress { received, total });
            reported = (Instant::now(), received);
        }
    }
    file.flush().await.map_err(|e| error::from_io(&e))?;
    file.sync_all().await.map_err(|e| error::from_io(&e))?;
    progress.send_replace(Progress { received, total });
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_needs_a_known_size() {
        assert_eq!(
            Progress {
                received: 5,
                total: None
            }
            .percent(),
            None
        );
        assert_eq!(
            Progress {
                received: 5,
                total: Some(0)
            }
            .percent(),
            None
        );
        assert_eq!(
            Progress {
                received: 21,
                total: Some(50)
            }
            .percent(),
            Some(42)
        );
        assert_eq!(
            Progress {
                received: 80,
                total: Some(50)
            }
            .percent(),
            Some(100)
        );
    }
}
