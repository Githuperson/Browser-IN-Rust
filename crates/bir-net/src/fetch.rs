//! A very small HTTP GET client, used only for refreshing filter lists and favicons.
//!
//! Everything is done on a worker thread with a hard deadline enforced by
//! `mpsc::recv_timeout` rather than by the HTTP client's own timeout API. That keeps
//! our dependency on `ureq` down to two functions, and — more importantly — guarantees
//! a hung list server can never stall the browser: we simply stop waiting for the
//! thread and carry on.

use std::{
  sync::mpsc::{channel, RecvTimeoutError},
  thread,
  time::Duration,
};

use crate::{Error, Result};

/// Default deadline for a background fetch.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest response we will buffer. A "filter list" of more than 32 MB is not a filter
/// list.
pub const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// The browser's own user agent, used for our (few) internal requests.
pub const USER_AGENT: &str = concat!("BIR/", env!("CARGO_PKG_VERSION"), " (browser-in-rust)");

/// Fetch `url` as a UTF-8 string, giving up after `timeout`.
///
/// Returns `Err` on transport failure, non-2xx status, or deadline. The worker thread
/// is deliberately detached: a socket stuck in `connect()` has no reliable cancel, and
/// leaking a blocked thread once in a while is cheaper than blocking shutdown on it.
pub fn get_text(url: &str, timeout: Duration, max_bytes: usize) -> Result<String> {
  let (tx, rx) = channel();
  let owned = url.to_string();
  thread::Builder::new()
    .name("bir-fetch".into())
    .spawn(move || {
      let outcome = blocking_get(&owned, max_bytes);
      let _ = tx.send(outcome);
    })
    .map_err(|e| Error::Network(format!("failed to spawn fetch thread: {e}")))?;

  match rx.recv_timeout(timeout) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => Err(Error::Network(format!(
      "timed out after {}s fetching {url}",
      timeout.as_secs()
    ))),
    Err(RecvTimeoutError::Disconnected) => Err(Error::Network(format!(
      "fetch thread died while fetching {url}"
    ))),
  }
}

fn blocking_get(url: &str, max_bytes: usize) -> Result<String> {
  let mut response = ureq::get(url)
    .header("User-Agent", USER_AGENT)
    .header("Accept", "text/plain, */*")
    .call()
    .map_err(|e| Error::Network(format!("{url}: {e}")))?;

  let status = response.status();
  if !status.is_success() {
    return Err(Error::Network(format!("{url}: HTTP {status}")));
  }

  let text = response
    .body_mut()
    .with_config()
    .limit(max_bytes as u64)
    .read_to_string()
    .map_err(|e| Error::Network(format!("{url}: reading body: {e}")))?;
  Ok(text)
}

/// Fetch raw bytes (favicons). Same deadline semantics as [`get_text`].
pub fn get_bytes(url: &str, timeout: Duration, max_bytes: usize) -> Result<Vec<u8>> {
  let (tx, rx) = channel();
  let owned = url.to_string();
  thread::Builder::new()
    .name("bir-fetch-bytes".into())
    .spawn(move || {
      let outcome = blocking_get_bytes(&owned, max_bytes);
      let _ = tx.send(outcome);
    })
    .map_err(|e| Error::Network(format!("failed to spawn fetch thread: {e}")))?;

  match rx.recv_timeout(timeout) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => Err(Error::Network(format!("timed out fetching {url}"))),
    Err(RecvTimeoutError::Disconnected) => Err(Error::Network(format!("fetch thread died"))),
  }
}

fn blocking_get_bytes(url: &str, max_bytes: usize) -> Result<Vec<u8>> {
  let mut response = ureq::get(url)
    .header("User-Agent", USER_AGENT)
    .call()
    .map_err(|e| Error::Network(format!("{url}: {e}")))?;

  if !response.status().is_success() {
    return Err(Error::Network(format!("{url}: HTTP {}", response.status())));
  }

  response
    .body_mut()
    .with_config()
    .limit(max_bytes as u64)
    .read_to_vec()
    .map_err(|e| Error::Network(format!("{url}: reading body: {e}")) )
}
