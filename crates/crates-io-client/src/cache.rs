//! Cache entries shared by every upstream fetch.

use std::{
    any::Any,
    sync::{Arc, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use tokio::time::Instant;

/// A cached HTTP response together with everything needed to revalidate it.
#[derive(Debug)]
pub struct CachedBody {
    /// The (already content-decoded) response body.
    pub body: Bytes,
    /// The status code the body was served with.
    pub status: u16,
    /// `ETag` header, when the origin supplied one.
    pub etag: Option<Box<str>>,
    /// `Last-Modified` header, when the origin supplied one.
    pub last_modified: Option<Box<str>>,
    /// The URL the body was ultimately served from, after any redirects.
    pub final_url: Box<str>,
    /// When this entry was stored or last revalidated.
    stored_at: Instant,
    /// How long the entry may be served without contacting the origin.
    fresh_for: Duration,
    /// Lazily computed, type-erased projection of [`CachedBody::body`].
    ///
    /// Parsing a payload is often more expensive than transferring it, so the
    /// parsed form is memoized here and survives `304` revalidation. A crate's
    /// sparse-index document is therefore parsed exactly once per distinct
    /// payload no matter how many tool calls read it.
    derived: OnceLock<Arc<dyn Any + Send + Sync>>,
}

impl CachedBody {
    /// Store a freshly fetched body.
    #[must_use]
    pub fn new(
        body: Bytes,
        status: u16,
        etag: Option<Box<str>>,
        last_modified: Option<Box<str>>,
        final_url: Box<str>,
        fresh_for: Duration,
    ) -> Self {
        Self {
            body,
            status,
            etag,
            last_modified,
            final_url,
            stored_at: Instant::now(),
            fresh_for,
            derived: OnceLock::new(),
        }
    }

    /// Whether the entry may still be served without contacting the origin.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.stored_at.elapsed() < self.fresh_for
    }

    /// Whether the entry carries a validator usable for a conditional request.
    #[must_use]
    pub fn has_validator(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }

    /// Approximate heap cost of this entry, used to bound the cache by bytes.
    #[must_use]
    pub fn weight(&self) -> u32 {
        let bytes = self.body.len()
            + self.final_url.len()
            + self.etag.as_ref().map_or(0, |e| e.len())
            + self.last_modified.as_ref().map_or(0, |l| l.len())
            + std::mem::size_of::<Self>();
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }

    /// Rebuild this entry after a `304 Not Modified`, carrying the memoized
    /// projection across so that revalidation never forces a reparse.
    #[must_use]
    pub fn revalidated(&self, fresh_for: Duration) -> Self {
        let derived = OnceLock::new();
        if let Some(value) = self.derived.get() {
            let _ = derived.set(Arc::clone(value));
        }
        Self {
            body: self.body.clone(),
            status: self.status,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            final_url: self.final_url.clone(),
            stored_at: Instant::now(),
            fresh_for,
            derived,
        }
    }

    /// Return the memoized projection of this body, computing it on first use.
    ///
    /// `project` may run more than once if several tasks race, but only one
    /// result is retained and every later caller shares it.
    ///
    /// # Errors
    ///
    /// Propagates whatever `project` returns.
    pub fn derive<T, E, F>(&self, project: F) -> Result<Arc<T>, E>
    where
        T: Send + Sync + 'static,
        F: FnOnce(&Bytes) -> Result<T, E>,
    {
        if let Some(existing) = self.derived.get()
            && let Ok(typed) = Arc::clone(existing).downcast::<T>()
        {
            return Ok(typed);
        }
        let value = Arc::new(project(&self.body)?);
        let _ = self
            .derived
            .set(Arc::clone(&value) as Arc<dyn Any + Send + Sync>);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fresh_for: Duration) -> CachedBody {
        CachedBody::new(
            Bytes::from_static(b"42"),
            200,
            Some("\"abc\"".into()),
            None,
            "https://example.invalid/x".into(),
            fresh_for,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_expires_after_the_ttl() {
        let cached = entry(Duration::from_secs(60));
        assert!(cached.is_fresh());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(!cached.is_fresh());
    }

    #[tokio::test]
    async fn derive_runs_the_projection_once() {
        let cached = entry(Duration::from_secs(60));
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let parse = |b: &Bytes| -> Result<u64, std::num::ParseIntError> {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::str::from_utf8(b).expect("ascii").parse()
        };

        assert_eq!(*cached.derive(parse).expect("parses"), 42);
        assert_eq!(*cached.derive(parse).expect("parses"), 42);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn revalidation_refreshes_the_clock_and_keeps_the_projection() {
        let cached = entry(Duration::from_secs(10));
        let parsed = cached
            .derive(|b: &Bytes| std::str::from_utf8(b).expect("ascii").parse::<u64>())
            .expect("parses");
        assert_eq!(*parsed, 42);

        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(!cached.is_fresh());

        let refreshed = cached.revalidated(Duration::from_secs(10));
        assert!(refreshed.is_fresh());

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let reparsed = refreshed
            .derive(|b: &Bytes| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::str::from_utf8(b).expect("ascii").parse::<u64>()
            })
            .expect("parses");
        assert_eq!(*reparsed, 42);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the memoized projection should survive revalidation"
        );
    }
}
