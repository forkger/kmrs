//! Outbound generic JSON webhooks.
//!
//! kmrs enhancement with no Java equivalent: every [`DomainEvent`]
//! is POSTed as `{event, timestamp, data}` to the configured URLs. The dispatcher
//! subscribes to the broadcast bus directly (same pattern as `search_index::consume_events`);
//! it never goes through the SSE wire format. Event names and `data` shapes reuse the
//! SSE DTOs (`crate::sse::dto`), minus the per-user/admin scoping — webhooks see all events,
//! including `LibraryScanned`, which SSE withholds. `UserUpdated` without session expiry
//! is skipped exactly like in SSE; only real session ends (`expire_session` or deletion)
//! emit `SessionExpired`. Events with
//! a series/book context additionally carry the full `series`/`book` DTOs (same shape as
//! the REST API, null when the row is gone): `series` on SeriesAdded/SeriesUpdated,
//! `book` on BookAdded/BookUpdated; everything else stays id-only. Library/ReadList/
//! Collection/User events carry their names directly from the event object (`libraryName`,
//! `readListName`, `collectionName`, `userEmail`) with no extra lookup.
//!
//! Every request carries `X-Kmrs-Event` with the event name. When a signing secret is
//! configured, the request additionally carries `X-Kmrs-Signature: t=<timestamp>,v1=<hex>`,
//! where `v1` is HMAC-SHA256 over `<timestamp>.<raw-body-bytes>`. Receivers should check
//! the timestamp against a small window (e.g. ±5 minutes) to reject replays.
//!
//! Delivery is best-effort with bounded retries: 429/5xx responses and transport errors
//! are retried up to 3 times with 1s/2s/4s exponential backoff (a larger `Retry-After`
//! wins, capped at 60s); other 4xx responses are dropped immediately. Pending deliveries
//! wait in a bounded in-memory queue (1024); when full, the newest event is dropped with
//! a warning. Nothing is persisted: retries in flight are lost on restart.

use crate::events::DomainEvent;
use crate::state::AppState;
use komga_db::dao::book::BookDao;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const QUEUE_CAPACITY: usize = 1024;
const WORKER_COUNT: usize = 4;
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Subscribes to the domain event bus and POSTs each event to the configured URLs.
///
/// Fire-and-forget: the bus loop only enqueues and never blocks on delivery; failures
/// log a warning and never stall the bus or the scan pipeline. With no endpoints configured
/// this spawns a task that returns immediately.
pub fn consume_events(state: AppState) -> tokio::task::JoinHandle<()> {
    let endpoints = state.config.webhooks.endpoints.clone();
    let timeout = state.config.webhooks.timeout;
    tokio::spawn(async move {
        if endpoints.is_empty() {
            return;
        }
        let client = match reqwest::Client::builder().timeout(timeout).build() {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("webhook client build failed, webhooks disabled: {e}");
                return;
            }
        };
        let (tx, rx) = tokio::sync::mpsc::channel::<QueuedDelivery>(QUEUE_CAPACITY);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..WORKER_COUNT {
            let rx = rx.clone();
            let client = client.clone();
            tokio::spawn(async move { worker(rx, client).await });
        }
        let mut receiver = state.events.subscribe();
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // Extract event name first (no DB lookups) to check filter before enrichment.
                    let Some(name) = event_name(&event) else {
                        continue;
                    };
                    // Skip if no endpoint subscribes to this event type.
                    let mut interested = false;
                    for endpoint in &endpoints {
                        if event_allowed(&endpoint.events, name) {
                            interested = true;
                            break;
                        }
                    }
                    if !interested {
                        continue;
                    }
                    // At least one endpoint wants this event: build payload and enrich.
                    let Some((_, mut data)) = event_payload(&state, &event).await else {
                        continue;
                    };
                    enrich_data(&state, &event, &mut data).await;
                    let body = serde_json::json!({
                        "event": name,
                        "timestamp": komga_core::time_codec::format_dto_datetime(
                            komga_core::time_codec::now_utc(),
                        ),
                        "data": data,
                    });
                    for endpoint in &endpoints {
                        if !event_allowed(&endpoint.events, name) {
                            continue;
                        }
                        let delivery = QueuedDelivery {
                            url: endpoint.url.clone(),
                            secret: endpoint.secret.clone(),
                            event_name: name,
                            body: body.clone(),
                        };
                        if tx.try_send(delivery).is_err() {
                            tracing::warn!(
                                "webhook queue full ({QUEUE_CAPACITY}), dropping {name} event for {}",
                                endpoint.url
                            );
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("webhook event consumer lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Returns the webhook event name for a domain event, or None if the event
/// should not produce a webhook (e.g. UserUpdated without session expiry).
fn event_name(event: &DomainEvent) -> Option<&'static str> {
    // Aligned with SSE: a routine user update invalidates nothing and yields no event;
    // only real session ends (explicit expiry or deletion) map to `SessionExpired`.
    if let DomainEvent::UserUpdated {
        expire_session: false,
        ..
    } = event
    {
        return None;
    }
    let name = match event {
        DomainEvent::LibraryAdded(_) => "LibraryAdded",
        DomainEvent::LibraryUpdated(_) => "LibraryChanged",
        DomainEvent::LibraryDeleted(_) => "LibraryDeleted",
        DomainEvent::LibraryScanned(_) => "LibraryScanned",
        DomainEvent::SeriesAdded(_) => "SeriesAdded",
        DomainEvent::SeriesUpdated(_) => "SeriesChanged",
        DomainEvent::SeriesDeleted(_) => "SeriesDeleted",
        DomainEvent::BookAdded(_) => "BookAdded",
        DomainEvent::BookUpdated(_) => "BookChanged",
        DomainEvent::BookDeleted(_) => "BookDeleted",
        DomainEvent::BookImported { .. } => "BookImported",
        DomainEvent::ReadListAdded(_) => "ReadListAdded",
        DomainEvent::ReadListUpdated(_) => "ReadListChanged",
        DomainEvent::ReadListDeleted(_) => "ReadListDeleted",
        DomainEvent::CollectionAdded(_) => "CollectionAdded",
        DomainEvent::CollectionUpdated(_) => "CollectionChanged",
        DomainEvent::CollectionDeleted(_) => "CollectionDeleted",
        DomainEvent::ReadProgressChanged(_) => "ReadProgressChanged",
        DomainEvent::ReadProgressDeleted(_) => "ReadProgressDeleted",
        DomainEvent::ReadProgressSeriesChanged { .. } => "ReadProgressSeriesChanged",
        DomainEvent::ReadProgressSeriesDeleted { .. } => "ReadProgressSeriesDeleted",
        DomainEvent::ThumbnailBookAdded(_) => "ThumbnailBookAdded",
        DomainEvent::ThumbnailBookDeleted(_) => "ThumbnailBookDeleted",
        DomainEvent::ThumbnailSeriesAdded(_) => "ThumbnailSeriesAdded",
        DomainEvent::ThumbnailSeriesDeleted(_) => "ThumbnailSeriesDeleted",
        DomainEvent::ThumbnailSeriesCollectionAdded(_) => "ThumbnailSeriesCollectionAdded",
        DomainEvent::ThumbnailSeriesCollectionDeleted(_) => "ThumbnailSeriesCollectionDeleted",
        DomainEvent::ThumbnailReadListAdded(_) => "ThumbnailReadListAdded",
        DomainEvent::ThumbnailReadListDeleted(_) => "ThumbnailReadListDeleted",
        // UserUpdated with expire_session=true -> SessionExpired (matches SSE)
        DomainEvent::UserUpdated { .. } => "SessionExpired",
        // UserDeleted is never published (no publisher in codebase, Java parity only).
        DomainEvent::UserDeleted(_) => return None,
        // kmrs-only events stay SSE-local: webhooks speak komga's event vocabulary,
        // and external consumers would not know these names.
        DomainEvent::SmartListAdded(_)
        | DomainEvent::SmartListUpdated(_)
        | DomainEvent::SmartListDeleted(_)
        | DomainEvent::SmartListThumbnailChanged(_)
        | DomainEvent::FileDeletionCompleted(_) => return None,
    };
    Some(name)
}

struct QueuedDelivery {
    url: String,
    secret: Option<String>,
    event_name: &'static str,
    body: serde_json::Value,
}

async fn worker(
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<QueuedDelivery>>>,
    client: reqwest::Client,
) {
    loop {
        let delivery = rx.lock().await.recv().await;
        let Some(delivery) = delivery else {
            break;
        };
        deliver_with_retry(&client, &delivery).await;
    }
}

async fn deliver_with_retry(client: &reqwest::Client, delivery: &QueuedDelivery) {
    let mut attempt: u32 = 0;
    loop {
        match post_once(client, delivery).await {
            AttemptOutcome::Delivered => return,
            AttemptOutcome::Retryable {
                message,
                retry_after,
            } if attempt < MAX_RETRIES => {
                let delay = retry_delay(attempt, retry_after);
                tracing::warn!(
                    "webhook POST to {} failed (attempt {}/{}): {message}; retrying in {delay:?}",
                    delivery.url,
                    attempt + 1,
                    MAX_RETRIES + 1,
                );
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            AttemptOutcome::Retryable { message, .. } => {
                tracing::warn!(
                    "webhook POST to {} failed after {} attempts, dropping: {message}",
                    delivery.url,
                    MAX_RETRIES + 1,
                );
                return;
            }
            AttemptOutcome::Permanent { message } => {
                tracing::warn!(
                    "webhook POST to {} rejected, dropping: {message}",
                    delivery.url
                );
                return;
            }
        }
    }
}

/// 1s/2s/4s exponential backoff; a larger `Retry-After` wins, capped at 60s so a
/// misbehaving receiver cannot park a worker indefinitely.
fn retry_delay(attempt: u32, retry_after: Option<std::time::Duration>) -> std::time::Duration {
    let backoff = std::time::Duration::from_secs(1 << attempt);
    match retry_after {
        Some(delay) if delay > backoff => delay.min(MAX_RETRY_AFTER),
        _ => backoff,
    }
}

enum AttemptOutcome {
    Delivered,
    Retryable {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    Permanent {
        message: String,
    },
}

async fn post_once(client: &reqwest::Client, delivery: &QueuedDelivery) -> AttemptOutcome {
    // Serialize once so the signature covers the exact bytes on the wire.
    let raw = match serde_json::to_vec(&delivery.body) {
        Ok(raw) => raw,
        Err(e) => {
            return AttemptOutcome::Permanent {
                message: format!("serialize failed: {e}"),
            };
        }
    };
    let mut request = client
        .post(&delivery.url)
        .header("Content-Type", "application/json")
        .header("X-Kmrs-Event", delivery.event_name)
        .body(raw.clone());
    if let Some(secret) = &delivery.secret {
        let timestamp = delivery
            .body
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let signature = signature_header(secret, timestamp, &raw);
        request = request.header("X-Kmrs-Signature", signature);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return AttemptOutcome::Retryable {
                message: e.to_string(),
                retry_after: None,
            };
        }
    };
    let status = response.status();
    if status.is_success() {
        return AttemptOutcome::Delivered;
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(parse_retry_after);
    // Read response body with a cap to avoid unbounded memory from misbehaving endpoints.
    // Accumulate raw bytes to avoid UTF-8 boundary panics when slicing.
    let mut buf = Vec::with_capacity(500);
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        if let Ok(bytes) = chunk {
            let take = 500usize.saturating_sub(buf.len());
            if take == 0 {
                break;
            }
            buf.extend_from_slice(&bytes[..take.min(bytes.len())]);
        }
    }
    let body = String::from_utf8_lossy(&buf);
    let message = format!("upstream returned {status}: {body}");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        AttemptOutcome::Retryable {
            message,
            retry_after,
        }
    } else {
        AttemptOutcome::Permanent { message }
    }
}

/// `Retry-After` in delta-seconds or HTTP-date form; unparsable falls back to the backoff.
fn parse_retry_after(value: &reqwest::header::HeaderValue) -> Option<std::time::Duration> {
    let s = value.to_str().ok()?.trim();
    // Try delta-seconds first (e.g., "120")
    if let Ok(secs) = s.parse::<u64>() {
        return Some(std::time::Duration::from_secs(secs));
    }
    // Try HTTP-date (RFC 7231, e.g., "Wed, 21 Oct 2015 07:28:00 GMT")
    if let Ok(dt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc2822) {
        let now = time::OffsetDateTime::now_utc();
        if dt > now {
            return Some((dt - now).unsigned_abs());
        }
    }
    None
}

/// Empty filter means all events; otherwise the event name must be listed.
fn event_allowed(filter: &[String], name: &str) -> bool {
    filter.is_empty() || filter.iter().any(|f| f == name)
}

/// `X-Kmrs-Signature` value: `t=<timestamp>,v1=<hex>`, with `v1` the HMAC-SHA256
/// of `<timestamp>.<raw-body-bytes>` under the configured secret.
fn signature_header(secret: &str, timestamp: &str, raw_body: &[u8]) -> String {
    let mut message = Vec::with_capacity(timestamp.len() + 1 + raw_body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.push(b'.');
    message.extend_from_slice(raw_body);
    format!(
        "t={timestamp},v1={}",
        hmac_sha256_hex(secret.as_bytes(), &message)
    )
}

/// HMAC-SHA256 (RFC 2104) over the vendored `sha2` crate, avoiding a new dependency
/// for a single call site. Verified against the RFC 4231 vectors in `hmac_vectors` below.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    for b in &key_block {
        inner.update([b ^ 0x36]);
    }
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    for b in &key_block {
        outer.update([b ^ 0x5c]);
    }
    outer.update(inner_hash);
    hex::encode(outer.finalize())
}

/// Maps a domain event to its webhook `(name, data)`. Names and shapes match the SSE
/// stream so external consumers can share parsing code; the SSE-only `TaskQueueStatus`
/// has no domain event behind it and is therefore never emitted here.
async fn event_payload(
    state: &AppState,
    event: &DomainEvent,
) -> Option<(&'static str, serde_json::Value)> {
    use crate::sse::dto::*;
    // Aligned with SSE: a routine user update invalidates nothing and yields no event;
    // only real session ends (explicit expiry or deletion) map to `SessionExpired`.
    if let DomainEvent::UserUpdated {
        expire_session: false,
        ..
    } = event
    {
        return None;
    }
    let value = match event {
        DomainEvent::LibraryAdded(l) => (
            "LibraryAdded",
            serde_json::to_value(LibrarySseDto {
                library_id: l.id.clone(),
            }),
        ),
        DomainEvent::LibraryUpdated(l) => (
            "LibraryChanged",
            serde_json::to_value(LibrarySseDto {
                library_id: l.id.clone(),
            }),
        ),
        DomainEvent::LibraryDeleted(l) => (
            "LibraryDeleted",
            serde_json::to_value(LibrarySseDto {
                library_id: l.id.clone(),
            }),
        ),
        DomainEvent::LibraryScanned(l) => (
            "LibraryScanned",
            serde_json::to_value(LibrarySseDto {
                library_id: l.id.clone(),
            }),
        ),

        DomainEvent::SeriesAdded(s) => (
            "SeriesAdded",
            serde_json::to_value(SeriesSseDto {
                series_id: s.id.clone(),
                library_id: s.library_id.clone(),
            }),
        ),
        DomainEvent::SeriesUpdated(s) => (
            "SeriesChanged",
            serde_json::to_value(SeriesSseDto {
                series_id: s.id.clone(),
                library_id: s.library_id.clone(),
            }),
        ),
        DomainEvent::SeriesDeleted(s) => (
            "SeriesDeleted",
            serde_json::to_value(SeriesSseDto {
                series_id: s.id.clone(),
                library_id: s.library_id.clone(),
            }),
        ),

        DomainEvent::BookAdded(b) => (
            "BookAdded",
            serde_json::to_value(BookSseDto {
                book_id: b.id.clone(),
                series_id: b.series_id.clone(),
                library_id: b.library_id.clone(),
            }),
        ),
        DomainEvent::BookUpdated(b) => (
            "BookChanged",
            serde_json::to_value(BookSseDto {
                book_id: b.id.clone(),
                series_id: b.series_id.clone(),
                library_id: b.library_id.clone(),
            }),
        ),
        DomainEvent::BookDeleted(b) => (
            "BookDeleted",
            serde_json::to_value(BookSseDto {
                book_id: b.id.clone(),
                series_id: b.series_id.clone(),
                library_id: b.library_id.clone(),
            }),
        ),
        DomainEvent::BookImported {
            book,
            source_file,
            success,
            message,
        } => (
            "BookImported",
            serde_json::to_value(BookImportSseDto {
                book_id: book.as_ref().map(|b| b.id.clone()),
                source_file: source_file.clone(),
                success: *success,
                message: message.clone(),
            }),
        ),

        DomainEvent::ReadListAdded(r) => (
            "ReadListAdded",
            serde_json::to_value(ReadListSseDto {
                read_list_id: r.id.clone(),
                book_ids: r.book_ids.clone().into_values().collect(),
            }),
        ),
        DomainEvent::ReadListUpdated(r) => (
            "ReadListChanged",
            serde_json::to_value(ReadListSseDto {
                read_list_id: r.id.clone(),
                book_ids: r.book_ids.clone().into_values().collect(),
            }),
        ),
        DomainEvent::ReadListDeleted(r) => (
            "ReadListDeleted",
            serde_json::to_value(ReadListSseDto {
                read_list_id: r.id.clone(),
                book_ids: r.book_ids.clone().into_values().collect(),
            }),
        ),

        DomainEvent::CollectionAdded(c) => (
            "CollectionAdded",
            serde_json::to_value(CollectionSseDto {
                collection_id: c.id.clone(),
                series_ids: c.series_ids.clone(),
            }),
        ),
        DomainEvent::CollectionUpdated(c) => (
            "CollectionChanged",
            serde_json::to_value(CollectionSseDto {
                collection_id: c.id.clone(),
                series_ids: c.series_ids.clone(),
            }),
        ),
        DomainEvent::CollectionDeleted(c) => (
            "CollectionDeleted",
            serde_json::to_value(CollectionSseDto {
                collection_id: c.id.clone(),
                series_ids: c.series_ids.clone(),
            }),
        ),

        DomainEvent::ReadProgressChanged(p) => (
            "ReadProgressChanged",
            serde_json::to_value(ReadProgressSseDto {
                book_id: p.book_id.clone(),
                user_id: p.user_id.clone(),
            }),
        ),
        DomainEvent::ReadProgressDeleted(p) => (
            "ReadProgressDeleted",
            serde_json::to_value(ReadProgressSseDto {
                book_id: p.book_id.clone(),
                user_id: p.user_id.clone(),
            }),
        ),
        DomainEvent::ReadProgressSeriesChanged { series_id, user_id } => (
            "ReadProgressSeriesChanged",
            serde_json::to_value(ReadProgressSeriesSseDto {
                series_id: series_id.clone(),
                user_id: user_id.clone(),
            }),
        ),
        DomainEvent::ReadProgressSeriesDeleted { series_id, user_id } => (
            "ReadProgressSeriesDeleted",
            serde_json::to_value(ReadProgressSeriesSseDto {
                series_id: series_id.clone(),
                user_id: user_id.clone(),
            }),
        ),

        DomainEvent::ThumbnailBookAdded(t) => (
            "ThumbnailBookAdded",
            serde_json::to_value(ThumbnailBookSseDto {
                book_id: t.book_id.clone(),
                series_id: book_series_id(state, &t.book_id).await,
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailBookDeleted(t) => (
            "ThumbnailBookDeleted",
            serde_json::to_value(ThumbnailBookSseDto {
                book_id: t.book_id.clone(),
                series_id: book_series_id(state, &t.book_id).await,
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailSeriesAdded(t) => (
            "ThumbnailSeriesAdded",
            serde_json::to_value(ThumbnailSeriesSseDto {
                series_id: t.series_id.clone(),
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailSeriesDeleted(t) => (
            "ThumbnailSeriesDeleted",
            serde_json::to_value(ThumbnailSeriesSseDto {
                series_id: t.series_id.clone(),
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailSeriesCollectionAdded(t) => (
            "ThumbnailSeriesCollectionAdded",
            serde_json::to_value(ThumbnailSeriesCollectionSseDto {
                collection_id: t.collection_id.clone(),
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailSeriesCollectionDeleted(t) => (
            "ThumbnailSeriesCollectionDeleted",
            serde_json::to_value(ThumbnailSeriesCollectionSseDto {
                collection_id: t.collection_id.clone(),
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailReadListAdded(t) => (
            "ThumbnailReadListAdded",
            serde_json::to_value(ThumbnailReadListSseDto {
                read_list_id: t.read_list_id.clone(),
                selected: t.selected,
            }),
        ),
        DomainEvent::ThumbnailReadListDeleted(t) => (
            "ThumbnailReadListDeleted",
            serde_json::to_value(ThumbnailReadListSseDto {
                read_list_id: t.read_list_id.clone(),
                selected: t.selected,
            }),
        ),

        DomainEvent::UserUpdated { user, .. } => (
            "SessionExpired",
            serde_json::to_value(SessionExpiredDto {
                user_id: user.id.clone(),
            }),
        ),
        // UserDeleted is never published (no publisher in codebase, Java parity only).
        DomainEvent::UserDeleted(_) => return None,
        // kmrs-only events stay SSE-local (same reason as `event_name` above).
        DomainEvent::SmartListAdded(_)
        | DomainEvent::SmartListUpdated(_)
        | DomainEvent::SmartListDeleted(_)
        | DomainEvent::SmartListThumbnailChanged(_)
        | DomainEvent::FileDeletionCompleted(_) => return None,
    };
    let (name, data) = value;
    data.ok().map(|data| (name, data))
}

/// `ThumbnailBookSseDto.seriesId`: unresolved book ids become an empty string.
async fn book_series_id(state: &AppState, book_id: &str) -> String {
    let db = state.db.clone();
    let book_id = book_id.to_string();
    // per-event lookups wait on the read pool under load; keep them off the async workers
    tokio::task::spawn_blocking(move || {
        BookDao::new(db)
            .get_series_id_or_null(&book_id)
            .ok()
            .flatten()
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// Attaches DTOs on upsert events: `series` on SeriesAdded/SeriesUpdated, `book` on
/// BookAdded/BookUpdated (null when the row is gone). Everything else stays id-only.
/// Carried-over names that need no lookup (`libraryName`, `readListName`,
/// `collectionName`, `userEmail`) are attached as well.
/// Runs after the events filter, so skipped events cost no lookups.
async fn enrich_data(state: &AppState, event: &DomainEvent, data: &mut serde_json::Value) {
    match event {
        DomainEvent::LibraryAdded(l)
        | DomainEvent::LibraryUpdated(l)
        | DomainEvent::LibraryDeleted(l)
        | DomainEvent::LibraryScanned(l) => {
            insert_value(data, "libraryName", l.name.clone().into());
        }
        DomainEvent::SeriesAdded(s) | DomainEvent::SeriesUpdated(s) => {
            insert_value(data, "series", series_dto_value(state, &s.id).await);
        }
        DomainEvent::BookAdded(b) | DomainEvent::BookUpdated(b) => {
            insert_value(data, "book", book_dto_value(state, &b.id).await);
        }
        DomainEvent::ReadListAdded(r)
        | DomainEvent::ReadListUpdated(r)
        | DomainEvent::ReadListDeleted(r) => {
            insert_value(data, "readListName", r.name.clone().into());
        }
        DomainEvent::CollectionAdded(c)
        | DomainEvent::CollectionUpdated(c)
        | DomainEvent::CollectionDeleted(c) => {
            insert_value(data, "collectionName", c.name.clone().into());
        }
        DomainEvent::UserUpdated { user, .. } => {
            insert_value(data, "userEmail", user.email.clone().into());
        }
        _ => {}
    }
}

/// Full DTO for the event's series, or null when the row is gone. `user_id` is empty:
///
/// the DTO queries only LEFT JOIN per-user read progress, so no user context is needed
/// and nothing is filtered. Equivalent to an admin `GET /api/v1/series/{id}` with the
/// full (unredacted) `url`. The DTO needs its complete row set (series metadata plus
/// the aggregation row the scanner maintains); deleted or half-written rows yield null.
async fn series_dto_value(state: &AppState, series_id: &str) -> serde_json::Value {
    let db = state.db.clone();
    let series_id = series_id.to_string();
    // the DTO query is one of the heaviest reads; during an event storm it must not
    // occupy an async worker while waiting on the read pool
    tokio::task::spawn_blocking(move || {
        komga_db::dto_dao::series::SeriesDtoDao::new(db)
            .find_by_id(&series_id, "")
            .ok()
            .flatten()
            .and_then(|dto| serde_json::to_value(dto).ok())
            .unwrap_or(serde_json::Value::Null)
    })
    .await
    .unwrap_or(serde_json::Value::Null)
}

/// Full DTO for the event's book, or null when the row is gone. Same admin-scope
/// semantics as [`series_dto_value`]; equivalent to `GET /api/v1/books/{id}`.
async fn book_dto_value(state: &AppState, book_id: &str) -> serde_json::Value {
    let db = state.db.clone();
    let book_id = book_id.to_string();
    tokio::task::spawn_blocking(move || {
        komga_db::dto_dao::book::BookDtoDao::new(db)
            .find_by_id(&book_id, "")
            .ok()
            .flatten()
            .and_then(|dto| serde_json::to_value(dto).ok())
            .unwrap_or(serde_json::Value::Null)
    })
    .await
    .unwrap_or(serde_json::Value::Null)
}

fn insert_value(data: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(map) = data.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebhookEndpoint;
    use crate::state::test_kmrs_db;
    use axum::routing::post;
    use komga_core::model::book::Book;
    use komga_core::model::library::Library;
    use komga_core::model::series::Series;
    use komga_core::model::user::{KomgaUser, UserRole};
    use komga_core::time_codec::now_utc;
    use komga_db::pool::Database;
    use komga_db::{Migrator, Placeholders};
    use std::time::Duration;

    fn test_state() -> AppState {
        let db = Database::open_in_memory(true).unwrap();
        Migrator::new(&komga_db::main_migrations(), Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        let tasks_db = Database::open_in_memory(false).unwrap();
        Migrator::new(&komga_db::tasks_migrations(), Placeholders::default())
            .migrate(&tasks_db.rw().unwrap())
            .unwrap();
        let config = crate::config::ServerConfig::from_env();
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db()).0
    }

    fn with_webhooks(state: &AppState, endpoints: Vec<WebhookEndpoint>) -> AppState {
        let mut config = (*state.config).clone();
        config.webhooks.endpoints = endpoints;
        config.webhooks.timeout = Duration::from_secs(5);
        AppState {
            config: Arc::new(config),
            ..state.clone()
        }
    }

    fn sample_series(id: &str, library_id: &str) -> Series {
        Series {
            id: id.into(),
            name: "Berserk".into(),
            url: "file:/data/berserk/".into(),
            file_last_modified: now_utc(),
            library_id: library_id.into(),
            book_count: 0,
            deleted_date: None,
            oneshot: false,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    fn sample_book(id: &str, series_id: &str, library_id: &str) -> Book {
        Book {
            id: id.into(),
            name: "v01".into(),
            url: "file:/data/berserk/v01.cbz".into(),
            file_last_modified: now_utc(),
            series_id: series_id.into(),
            library_id: library_id.into(),
            file_size: 100,
            number: 0,
            file_hash: String::new(),
            file_hash_koreader: String::new(),
            deleted_date: None,
            oneshot: false,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    fn sample_library(id: &str) -> Library {
        Library {
            id: id.into(),
            name: "Manga".into(),
            root: "file:/data/manga/".into(),
            import_comicinfo_book: false,
            import_comicinfo_series: false,
            import_comicinfo_collection: false,
            import_comicinfo_readlist: false,
            import_comicinfo_series_append_volume: false,
            import_epub_book: false,
            import_epub_series: false,
            import_mylar_series: false,
            import_local_artwork: false,
            import_barcode_isbn: false,
            scan_force_modified_time: false,
            scan_interval: komga_core::model::library::ScanInterval::Daily,
            scan_on_startup: false,
            scan_cbx: true,
            scan_pdf: true,
            scan_epub: true,
            scan_directory_exclusions: vec![],
            repair_extensions: false,
            convert_to_cbz: false,
            empty_trash_after_scan: false,
            series_cover: komga_core::model::library::SeriesCover::First,
            hash_files: false,
            hash_pages: false,
            hash_koreader: false,
            analyze_dimensions: false,
            oneshots_directory: None,
            unavailable_date: None,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    fn sample_user() -> KomgaUser {
        KomgaUser {
            id: "u1".into(),
            email: "a@b.c".into(),
            password: "x".into(),
            roles: [UserRole::Admin].into_iter().collect(),
            shared_libraries_ids: Default::default(),
            shared_all_libraries: true,
            restrictions: Default::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        }
    }

    #[tokio::test]
    async fn payload_names_match_sse() {
        let state = test_state();
        let (name, data) = event_payload(
            &state,
            &DomainEvent::BookAdded(sample_book("b1", "s1", "l1")),
        )
        .await
        .unwrap();
        assert_eq!(name, "BookAdded");
        assert_eq!(
            data,
            serde_json::json!({"bookId":"b1","seriesId":"s1","libraryId":"l1"})
        );

        let (name, data) = event_payload(
            &state,
            &DomainEvent::SeriesUpdated(sample_series("s1", "l1")),
        )
        .await
        .unwrap();
        assert_eq!(name, "SeriesChanged");
        assert_eq!(data, serde_json::json!({"seriesId":"s1","libraryId":"l1"}));

        // LibraryScanned has no SSE equivalent but is emitted for automation.
        let (name, data) =
            event_payload(&state, &DomainEvent::LibraryScanned(sample_library("l1")))
                .await
                .unwrap();
        assert_eq!(name, "LibraryScanned");
        assert_eq!(data, serde_json::json!({"libraryId":"l1"}));

        // a routine user update invalidates nothing: no event, exactly like in SSE.
        assert!(event_payload(
            &state,
            &DomainEvent::UserUpdated {
                user: sample_user(),
                expire_session: false,
            },
        )
        .await
        .is_none());
        // only real session ends map to SessionExpired.
        let (name, data) = event_payload(
            &state,
            &DomainEvent::UserUpdated {
                user: sample_user(),
                expire_session: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(name, "SessionExpired");
        assert_eq!(data, serde_json::json!({"userId":"u1"}));
    }

    async fn payload_of(state: &AppState, event: DomainEvent) -> serde_json::Value {
        let (name, mut data) = event_payload(state, &event).await.unwrap();
        let _ = name;
        enrich_data(state, &event, &mut data).await;
        data
    }

    #[tokio::test]
    async fn enrichment_attaches_dtos_with_null_fallback() {
        let state = test_state();
        komga_db::dao::library::LibraryDao::new(state.db.clone())
            .insert(&sample_library("l1"))
            .unwrap();
        komga_db::dao::series::SeriesDao::new(state.db.clone())
            .insert(&sample_series("s1", "l1"))
            .unwrap();
        komga_db::dao::book::BookDao::new(state.db.clone())
            .insert(&sample_book("b1", "s1", "l1"))
            .unwrap();
        komga_db::dao::media::MediaDao::new(state.db.clone())
            .insert(&komga_core::model::media::Media {
                book_id: "b1".into(),
                status: komga_core::model::media::MediaStatus::Ready,
                media_type: None,
                comment: None,
                page_count: 0,
                pages: vec![],
                files: vec![],
                extension_class: None,
                extension_value: None,
                epub_divina_compatible: false,
                epub_is_kepub: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        komga_db::dao::book::BookMetadataDao::new(state.db.clone())
            .insert(&komga_core::model::book::BookMetadata {
                book_id: "b1".into(),
                title: "Berserk Deluxe v01".into(),
                summary: String::new(),
                number: String::new(),
                number_sort: 0.0,
                release_date: None,
                authors: vec![],
                notable_entities: vec![],
                tags: vec![],
                isbn: String::new(),
                links: vec![],
                title_lock: false,
                summary_lock: false,
                number_lock: false,
                number_sort_lock: false,
                release_date_lock: false,
                authors_lock: false,
                notable_entities_lock: false,
                tags_lock: false,
                isbn_lock: false,
                links_lock: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        komga_db::dao::series::SeriesMetadataDao::new(state.db.clone())
            .insert(&komga_core::model::series::SeriesMetadata {
                series_id: "s1".into(),
                status: komga_core::model::series::SeriesStatus::Ongoing,
                title: "Berserk Deluxe".into(),
                title_sort: String::new(),
                summary: String::new(),
                reading_direction: None,
                publisher: String::new(),
                age_rating: None,
                language: String::new(),
                genres: Default::default(),
                tags: Default::default(),
                total_book_count: None,
                sharing_labels: Default::default(),
                links: vec![],
                alternate_titles: vec![],
                status_lock: false,
                title_lock: false,
                title_sort_lock: false,
                summary_lock: false,
                reading_direction_lock: false,
                publisher_lock: false,
                age_rating_lock: false,
                language_lock: false,
                genres_lock: false,
                tags_lock: false,
                total_book_count_lock: false,
                sharing_labels_lock: false,
                links_lock: false,
                alternate_titles_lock: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
        komga_db::dao::series::BookMetadataAggregationDao::new(state.db.clone())
            .insert(&komga_core::model::series::BookMetadataAggregation {
                series_id: "s1".into(),
                authors: vec![],
                notable_entities: vec![],
                tags: Default::default(),
                release_date: None,
                summary: String::new(),
                summary_number: String::new(),
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();

        // series event carries the full series DTO mirroring the REST API.
        let data = payload_of(&state, DomainEvent::SeriesAdded(sample_series("s1", "l1"))).await;
        assert_eq!(data["series"]["id"], serde_json::json!("s1"));
        assert_eq!(
            data["series"]["metadata"]["title"],
            serde_json::json!("Berserk Deluxe")
        );
        assert!(data.get("book").is_none());

        // book event carries the book DTO only, no series object.
        let data = payload_of(
            &state,
            DomainEvent::BookAdded(sample_book("b1", "s1", "l1")),
        )
        .await;
        assert_eq!(data["book"]["id"], serde_json::json!("b1"));
        assert_eq!(
            data["book"]["metadata"]["title"],
            serde_json::json!("Berserk Deluxe v01")
        );
        assert_eq!(data["book"]["seriesId"], serde_json::json!("s1"));
        assert!(data.get("series").is_none());

        // read progress stays id-only: no DTO keys.
        let progress = komga_core::model::read_progress::ReadProgress {
            book_id: "b1".into(),
            user_id: "u1".into(),
            page: 3,
            completed: false,
            read_date: now_utc(),
            device_id: String::new(),
            device_name: String::new(),
            locator: None,
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let data = payload_of(&state, DomainEvent::ReadProgressChanged(progress)).await;
        assert_eq!(data, serde_json::json!({"bookId":"b1","userId":"u1"}));

        // unknown series ids on id-only events: nothing attached.
        let data = payload_of(
            &state,
            DomainEvent::ReadProgressSeriesChanged {
                series_id: "nope".into(),
                user_id: "ghost".into(),
            },
        )
        .await;
        assert_eq!(
            data,
            serde_json::json!({"seriesId":"nope","userId":"ghost"})
        );

        // deleted entities stay id-only even though the row is gone.
        let data = payload_of(
            &state,
            DomainEvent::BookDeleted(sample_book("b1", "s1", "l1")),
        )
        .await;
        assert_eq!(
            data,
            serde_json::json!({"bookId":"b1","seriesId":"s1","libraryId":"l1"})
        );
        assert!(data.get("book").is_none());
        let data = payload_of(
            &state,
            DomainEvent::SeriesDeleted(sample_series("s1", "l1")),
        )
        .await;
        assert_eq!(data, serde_json::json!({"seriesId":"s1","libraryId":"l1"}));
        assert!(data.get("series").is_none());

        // carried-over names need no lookup.
        let data = payload_of(&state, DomainEvent::LibraryAdded(sample_library("l1"))).await;
        assert_eq!(data["libraryName"], serde_json::json!("Manga"));
        let data = payload_of(
            &state,
            DomainEvent::ReadListAdded(komga_core::model::readlist::ReadList {
                id: "r1".into(),
                name: "Favorites".into(),
                summary: String::new(),
                ordered: true,
                book_ids: Default::default(),
                filtered: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            }),
        )
        .await;
        assert_eq!(data["readListName"], serde_json::json!("Favorites"));
        let data = payload_of(
            &state,
            DomainEvent::CollectionAdded(komga_core::model::collection::SeriesCollection {
                id: "c1".into(),
                name: "Classics".into(),
                ordered: false,
                series_ids: vec![],
                filtered: false,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            }),
        )
        .await;
        assert_eq!(data["collectionName"], serde_json::json!("Classics"));
    }

    #[test]
    fn event_filter() {
        assert!(event_allowed(&[], "BookAdded"));
        assert!(event_allowed(&["BookAdded".to_string()], "BookAdded"));
        assert!(!event_allowed(&["SeriesAdded".to_string()], "BookAdded"));
    }

    #[test]
    fn hmac_vectors() {
        // RFC 4231 §4.2, Test Cases 1–2 (HMAC-SHA-256).
        assert_eq!(
            hmac_sha256_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Key longer than block size (131 bytes of 0xaa) hashes down first; data "abc".
        assert_eq!(
            hmac_sha256_hex(&[0xaa; 131], b"abc"),
            "c21770e7a294fd85f9e8ad80b2d1e9cccb25d496015f8708e641358120f46976"
        );
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        let hdr = reqwest::header::HeaderValue::from_static("120");
        assert_eq!(
            parse_retry_after(&hdr),
            Some(std::time::Duration::from_secs(120))
        );
    }

    #[test]
    fn parse_retry_after_http_date() {
        use time::OffsetDateTime;
        let future = OffsetDateTime::now_utc() + time::Duration::seconds(5);
        let date_str = future
            .format(&time::format_description::well_known::Rfc2822)
            .unwrap()
            .replace("+0000", "GMT");
        let hdr = reqwest::header::HeaderValue::from_str(&date_str).unwrap();
        let parsed = parse_retry_after(&hdr);
        assert!(parsed.is_some(), "failed to parse: {}", date_str);
        let dur = parsed.unwrap();
        // Allow some slack for test execution time
        assert!(dur >= std::time::Duration::from_secs(3));
        assert!(dur <= std::time::Duration::from_secs(7));
    }

    #[derive(Debug, Clone)]
    struct Captured {
        event_header: Option<String>,
        signature_header: Option<String>,
        body: serde_json::Value,
    }

    async fn capture_server() -> (String, Arc<std::sync::Mutex<Vec<Captured>>>) {
        let received: Arc<std::sync::Mutex<Vec<Captured>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = received.clone();
        let app = axum::Router::new().route(
            "/hook",
            post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let inner = inner.clone();
                    async move {
                        inner.lock().unwrap().push(Captured {
                            event_header: headers
                                .get("x-kmrs-event")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            signature_header: headers
                                .get("x-kmrs-signature")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            body,
                        });
                        axum::http::StatusCode::OK
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/hook"), received)
    }

    async fn wait_for(
        received: &Arc<std::sync::Mutex<Vec<Captured>>>,
        count: usize,
    ) -> Vec<Captured> {
        for _ in 0..100 {
            {
                let guard = received.lock().unwrap();
                if guard.len() >= count {
                    return guard.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        received.lock().unwrap().clone()
    }

    /// Waits until the dispatcher task has subscribed to the bus
    /// (broadcast `send` fails with no receivers).
    async fn wait_subscribed(state: &AppState) {
        for _ in 0..100 {
            if state.events.receiver_count() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("webhook task did not subscribe in time");
    }

    #[tokio::test]
    async fn posts_envelope_to_configured_urls() {
        let (url, received) = capture_server().await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        state
            .events
            .send(DomainEvent::BookAdded(sample_book("b1", "s1", "l1")))
            .unwrap();

        let bodies = wait_for(&received, 1).await;
        handle.abort();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].body["event"], "BookAdded");
        assert_eq!(bodies[0].body["data"]["bookId"], serde_json::json!("b1"));
        // no DB rows: DTO objects are null.
        assert!(bodies[0].body["data"]["book"].is_null());
        assert!(bodies[0].body["data"]["series"].is_null());
        assert!(bodies[0].body["timestamp"].is_string());
        // the event header is always sent; nothing is signed without a secret.
        assert_eq!(bodies[0].event_header.as_deref(), Some("BookAdded"));
        assert_eq!(bodies[0].signature_header, None);
    }

    #[tokio::test]
    async fn signed_delivery_carries_verifiable_signature() {
        let (url, received) = capture_server().await;
        let mut state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        {
            let mut config = (*state.config).clone();
            config.webhooks.endpoints[0].secret = Some("s3cret".to_string());
            state = AppState {
                config: Arc::new(config),
                ..state
            };
        }
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        state
            .events
            .send(DomainEvent::BookAdded(sample_book("b1", "s1", "l1")))
            .unwrap();

        let bodies = wait_for(&received, 1).await;
        handle.abort();
        assert_eq!(bodies.len(), 1);
        let captured = &bodies[0];
        assert_eq!(captured.event_header.as_deref(), Some("BookAdded"));
        let signature = captured.signature_header.as_deref().unwrap();
        let timestamp = captured.body["timestamp"].as_str().unwrap();
        assert!(signature.starts_with(&format!("t={timestamp},v1=")));
        // the signature verifies against the received bytes and secret.
        let raw = serde_json::to_vec(&captured.body).unwrap();
        assert_eq!(signature, signature_header("s3cret", timestamp, &raw));
        // and it does not verify under a different secret.
        assert_ne!(signature, signature_header("wrong", timestamp, &raw));
    }

    #[tokio::test]
    async fn events_filter_skips_unlisted() {
        let (url, received) = capture_server().await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec!["SeriesAdded".to_string()],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        state
            .events
            .send(DomainEvent::BookAdded(sample_book("b1", "s1", "l1")))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.abort();
        assert!(received.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_without_urls() {
        let state = with_webhooks(&test_state(), vec![]);
        let handle = consume_events(state.clone());
        // no subscriber is ever created; the send has no receivers.
        let _ = state
            .events
            .send(DomainEvent::BookAdded(sample_book("b1", "s1", "l1")));
        // returns immediately without subscribing; nothing is delivered.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn retry_delays() {
        use std::time::Duration as StdDuration;
        assert_eq!(retry_delay(0, None), StdDuration::from_secs(1));
        assert_eq!(retry_delay(1, None), StdDuration::from_secs(2));
        assert_eq!(retry_delay(2, None), StdDuration::from_secs(4));
        // a larger Retry-After wins, capped at 60s.
        assert_eq!(
            retry_delay(0, Some(StdDuration::from_secs(30))),
            StdDuration::from_secs(30)
        );
        assert_eq!(
            retry_delay(0, Some(StdDuration::from_secs(3600))),
            StdDuration::from_secs(60)
        );
        // a smaller Retry-After falls back to the backoff.
        assert_eq!(
            retry_delay(2, Some(StdDuration::from_secs(1))),
            StdDuration::from_secs(4)
        );
    }

    /// Scripted responder: pops one `(status, retry-after)` per request, 200 when
    /// exhausted, and records attempts plus captured requests.
    struct Scripted {
        plan:
            std::sync::Mutex<std::collections::VecDeque<(axum::http::StatusCode, Option<String>)>>,
        attempts: std::sync::atomic::AtomicUsize,
        received: std::sync::Mutex<Vec<Captured>>,
    }

    async fn script_server(
        plan: Vec<(axum::http::StatusCode, Option<String>)>,
    ) -> (String, Arc<Scripted>) {
        let scripted = Arc::new(Scripted {
            plan: std::sync::Mutex::new(plan.into()),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            received: std::sync::Mutex::new(Vec::new()),
        });
        let inner = scripted.clone();
        let app = axum::Router::new().route(
            "/hook",
            post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let inner = inner.clone();
                    async move {
                        inner
                            .attempts
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        inner.received.lock().unwrap().push(Captured {
                            event_header: headers
                                .get("x-kmrs-event")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            signature_header: headers
                                .get("x-kmrs-signature")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            body,
                        });
                        let (status, retry_after) = inner
                            .plan
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or((axum::http::StatusCode::OK, None));
                        let mut response_headers = axum::http::HeaderMap::new();
                        if let Some(value) = retry_after {
                            response_headers.insert(
                                "retry-after",
                                value.parse().expect("scripted retry-after"),
                            );
                        }
                        (status, response_headers, "")
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/hook"), scripted)
    }

    async fn wait_attempts(scripted: &Scripted, count: usize) {
        for _ in 0..500 {
            if scripted.attempts.load(std::sync::atomic::Ordering::SeqCst) >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {count} attempts");
    }

    fn send_book_added(state: &AppState) {
        state
            .events
            .send(DomainEvent::BookAdded(sample_book("b1", "s1", "l1")))
            .unwrap();
    }

    #[tokio::test]
    async fn retries_server_errors_then_delivers() {
        use axum::http::StatusCode;
        let (url, scripted) = script_server(vec![
            (StatusCode::INTERNAL_SERVER_ERROR, None),
            (StatusCode::BAD_GATEWAY, None),
        ])
        .await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        send_book_added(&state);

        // two failures (1s + 2s backoff) then success on the third attempt.
        wait_attempts(&scripted, 3).await;
        handle.abort();
        assert_eq!(
            scripted.attempts.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
        let received = scripted.received.lock().unwrap();
        assert_eq!(received.len(), 3);
        assert_eq!(received[2].body["event"], "BookAdded");
    }

    #[tokio::test]
    async fn client_errors_are_not_retried() {
        use axum::http::StatusCode;
        let (url, scripted) = script_server(vec![(StatusCode::BAD_REQUEST, None)]).await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        send_book_added(&state);

        // sleep past the first 1s backoff to prove no second attempt happens.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        handle.abort();
        assert_eq!(
            scripted.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn retry_after_header_is_respected() {
        use axum::http::StatusCode;
        let (url, scripted) =
            script_server(vec![(StatusCode::TOO_MANY_REQUESTS, Some("2".to_string()))]).await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        let start = tokio::time::Instant::now();
        send_book_added(&state);

        wait_attempts(&scripted, 2).await;
        let elapsed = start.elapsed();
        handle.abort();
        assert_eq!(
            scripted.attempts.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(
            elapsed >= Duration::from_millis(1500),
            "retry-after not respected, elapsed: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn drops_after_max_retries() {
        use axum::http::StatusCode;
        let (url, scripted) = script_server(vec![
            (StatusCode::INTERNAL_SERVER_ERROR, None),
            (StatusCode::BAD_GATEWAY, None),
            (StatusCode::SERVICE_UNAVAILABLE, None),
            (StatusCode::GATEWAY_TIMEOUT, None), // 4th attempt should never happen
        ])
        .await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        send_book_added(&state);

        // Wait for 3 retry attempts + initial = 4 total (MAX_RETRIES=3 means 3 retries after initial)
        wait_attempts(&scripted, 4).await;
        // Give it a bit more time to ensure no 5th attempt
        tokio::time::sleep(Duration::from_millis(500)).await;
        handle.abort();
        let attempts = scripted.attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            attempts, 4,
            "should attempt exactly 4 times (initial + 3 retries), got {attempts}"
        );
        let received = scripted.received.lock().unwrap();
        assert_eq!(received.len(), 4);
    }

    #[tokio::test]
    async fn retry_after_http_date_is_respected() {
        use axum::http::StatusCode;
        use time::OffsetDateTime;
        // IMF-fixdate format (e.g., "Wed, 21 Oct 2015 07:28:00 GMT")
        // Use 5s to give plenty of margin over the 1s backoff
        let future = OffsetDateTime::now_utc() + time::Duration::seconds(5);
        // reformat as IMF-fixdate ("GMT" zone) to look like a real HTTP-date
        let retry_after_date = future
            .format(&time::format_description::well_known::Rfc2822)
            .unwrap()
            .replace("+0000", "GMT");
        let (url, scripted) = script_server(vec![(
            StatusCode::TOO_MANY_REQUESTS,
            Some(retry_after_date),
        )])
        .await;
        let state = with_webhooks(
            &test_state(),
            vec![WebhookEndpoint {
                url,
                events: vec![],
                secret: None,
            }],
        );
        let handle = consume_events(state.clone());
        wait_subscribed(&state).await;
        let start = tokio::time::Instant::now();
        send_book_added(&state);

        wait_attempts(&scripted, 2).await;
        let elapsed = start.elapsed();
        handle.abort();
        let attempts = scripted.attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(attempts, 2, "expected 2 attempts, got {}", attempts);
        // ~5s when honored vs ~1s on fallback to backoff: 2s separates the two cleanly,
        // a looser threshold could pass on scheduling jitter alone.
        assert!(
            elapsed >= Duration::from_secs(2),
            "retry-after HTTP-date not respected (elapsed={elapsed:?})"
        );
    }
}
