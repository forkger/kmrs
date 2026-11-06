//! `OpdsController.kt` and `OpdsCommonController.kt`: the OPDS v1.2 API (Atom XML).
//!
//! The Atom documents are serialized by hand to match Jackson XML's output exactly:
//! no XML declaration, only `xmlns` on the root (the pse namespace is declared inline
//! on each page-streaming link), `type`/`rel`/`href` attribute order for plain links,
//! and Jackson's escaping rules.

use crate::api::restriction;
use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::http::base_url::{base_url_from_headers, path_segment};
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::{routing, Router};
use komga_core::dto::book::BookDto;
use komga_core::model::library::Library;
use komga_core::model::media::{Media, MediaStatus};
use komga_core::model::thumbnail::ThumbnailType;
use komga_core::model::user::UserRole;
use komga_core::search::*;
use komga_db::dao::library::LibraryDao;
use komga_db::dao::media::MediaDao;
use komga_db::dto_dao::book::BookDtoDao;
use komga_db::dto_dao::collection::CollectionDtoDao;
use komga_db::dto_dao::readlist::ReadListDtoDao;
use komga_db::dto_dao::referential::ReferentialDao;
use komga_db::dto_dao::series::SeriesDtoDao;
use komga_db::dto_dao::{DtoPage, PageRequest, SortOrder as DbSortOrder};
use komga_media::container;
use komga_media::image::{self, ImageType};
use std::collections::BTreeSet;
use time::OffsetDateTime;

/// `ZonedDateTime.now()` (system zone), used for feed `updated` where the Kotlin side has no entity time.
fn now_zoned() -> OffsetDateTime {
    komga_core::time_codec::to_zoned_date_time(komga_core::time_codec::now_utc())
}

/// `LocalDateTime.atZone(ZoneId.systemDefault())`: wall-clock preserved, offset re-tagged.
fn at_zone(dt: OffsetDateTime) -> OffsetDateTime {
    dt.replace_offset(komga_core::time_codec::system_offset_for_wall_clock(dt))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/opds/v1.2/catalog", routing::get(get_catalog))
        .route("/opds/v1.2/search", routing::get(get_search))
        .route("/opds/v1.2/ondeck", routing::get(get_on_deck))
        .route("/opds/v1.2/keep-reading", routing::get(get_keep_reading))
        .route("/opds/v1.2/series", routing::get(get_all_series))
        .route("/opds/v1.2/series/latest", routing::get(get_latest_series))
        .route("/opds/v1.2/books/latest", routing::get(get_latest_books))
        .route("/opds/v1.2/libraries", routing::get(get_libraries))
        .route("/opds/v1.2/collections", routing::get(get_collections))
        .route("/opds/v1.2/readlists", routing::get(get_readlists))
        .route("/opds/v1.2/smart-lists", routing::get(get_smart_lists))
        .route(
            "/opds/v1.2/smart-lists/{id}",
            routing::get(get_one_smart_list),
        )
        .route("/opds/v1.2/publishers", routing::get(get_publishers))
        .route("/opds/v1.2/series/{id}", routing::get(get_one_series))
        .route("/opds/v1.2/libraries/{id}", routing::get(get_one_library))
        .route(
            "/opds/v1.2/collections/{id}",
            routing::get(get_one_collection),
        )
        .route("/opds/v1.2/readlists/{id}", routing::get(get_one_readlist))
        .route(
            "/opds/v1.2/books/{bookId}/thumbnail/small",
            routing::get(get_book_thumbnail_small),
        )
        .route(
            "/opds/v1.2/books/{bookId}/thumbnail",
            routing::get(get_book_thumbnail),
        )
        .route(
            "/opds/v1.2/books/{bookId}/pages/{pageNumber}",
            routing::get(get_book_page_opds),
        )
}

// region Atom model

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const PSE_NS: &str = "http://vaemendis.net/opds-pse/ns";
const TYPE_NAV: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";
const TYPE_ACQ: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";
const REL_IMAGE: &str = "http://opds-spec.org/image";
const REL_IMAGE_THUMBNAIL: &str = "http://opds-spec.org/image/thumbnail";
const REL_ACQUISITION: &str = "http://opds-spec.org/acquisition";
const REL_PAGE_STREAMING: &str = "http://vaemendis.net/opds-pse/stream";

struct AtomLink {
    type_: String,
    rel: String,
    href: String,
    pse_count: Option<i32>,
    pse_last_read: Option<i32>,
    pse_last_read_date: Option<OffsetDateTime>,
}

impl AtomLink {
    fn plain(type_: &str, rel: &str, href: String) -> Self {
        Self {
            type_: type_.to_string(),
            rel: rel.to_string(),
            href,
            pse_count: None,
            pse_last_read: None,
            pse_last_read_date: None,
        }
    }

    fn nav(rel: &str, href: String) -> Self {
        Self::plain(TYPE_NAV, rel, href)
    }

    fn image(media_type: &str, href: String) -> Self {
        Self::plain(media_type, REL_IMAGE, href)
    }

    fn image_thumbnail(media_type: &str, href: String) -> Self {
        Self::plain(media_type, REL_IMAGE_THUMBNAIL, href)
    }

    fn file_acquisition(media_type: Option<&str>, href: String) -> Self {
        Self::plain(
            media_type.unwrap_or("application/octet-stream"),
            REL_ACQUISITION,
            href,
        )
    }

    fn page_streaming(
        media_type: &str,
        href: String,
        count: i32,
        last_read: Option<i32>,
        last_read_date: Option<OffsetDateTime>,
    ) -> Self {
        Self {
            type_: media_type.to_string(),
            rel: REL_PAGE_STREAMING.to_string(),
            href,
            pse_count: Some(count),
            pse_last_read: last_read,
            pse_last_read_date: last_read_date,
        }
    }
}

struct EntryNav {
    title: String,
    updated: OffsetDateTime,
    id: String,
    content: String,
    link: AtomLink,
}

struct EntryAcq {
    title: String,
    updated: OffsetDateTime,
    id: String,
    content: String,
    authors: Vec<String>,
    links: Vec<AtomLink>,
}

enum Entry {
    Nav(EntryNav),
    Acq(EntryAcq),
}

struct Feed {
    id: String,
    title: String,
    updated: OffsetDateTime,
    entries: Vec<Entry>,
    links: Vec<AtomLink>,
}

// endregion

// region XML writer

fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn esc_attr(s: &str) -> String {
    esc_text(s)
        .replace('"', "&quot;")
        .replace('\n', "&#xA;")
        .replace('\r', "&#xD;")
        .replace('\t', "&#x9;")
}

/// Jackson `ISO_OFFSET_DATE_TIME` (delegates to `time_codec::format_offset_date_time`).
fn format_updated(dt: OffsetDateTime) -> String {
    komga_core::time_codec::format_offset_date_time(dt)
}

fn push_el(out: &mut String, name: &str, value: &str) {
    // StAX writes start+characters+end, so even empty elements render as <name></name>
    out.push_str(&format!("<{name}>{}</{name}>", esc_text(value)));
}

fn write_link(out: &mut String, link: &AtomLink) {
    if let Some(count) = link.pse_count {
        // Jackson XML declares the pse namespace inline on the link, not on the feed root;
        // attribute order is href, xmlns:pse, pse:*, type, rel (verified against Java komga)
        out.push_str(&format!(
            "<link href=\"{}\" xmlns:pse=\"{PSE_NS}\" pse:count=\"{count}\"",
            esc_attr(&link.href)
        ));
        if let Some(last_read) = link.pse_last_read {
            out.push_str(&format!(" pse:lastRead=\"{last_read}\""));
        }
        if let Some(date) = link.pse_last_read_date {
            out.push_str(&format!(
                " pse:lastReadDate=\"{}\"",
                format_pse_last_read_date(date)
            ));
        }
        out.push_str(&format!(
            " type=\"{}\" rel=\"{}\"/>",
            esc_attr(&link.type_),
            esc_attr(&link.rel)
        ));
        return;
    }
    out.push_str("<link");
    out.push_str(&format!(" type=\"{}\"", esc_attr(&link.type_)));
    out.push_str(&format!(" rel=\"{}\"", esc_attr(&link.rel)));
    out.push_str(&format!(" href=\"{}\"", esc_attr(&link.href)));
    out.push_str("/>");
}

/// `yyyy-MM-dd'T'HH:mm:ss'Z'` from the `@JsonFormat` on `OpdsLinkPageStreaming.lastReadDate`:
/// second precision, literal Z (the value is a UTC LocalDateTime).
fn format_pse_last_read_date(dt: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
}

fn write_feed(feed: &Feed) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(&format!("<feed xmlns=\"{ATOM_NS}\">"));
    push_el(&mut out, "id", &feed.id);
    push_el(&mut out, "title", &feed.title);
    push_el(&mut out, "updated", &format_updated(feed.updated));
    out.push_str("<author><name>Komga</name><uri>https://github.com/gotson/komga</uri></author>");
    for link in &feed.links {
        write_link(&mut out, link);
    }
    for entry in &feed.entries {
        match entry {
            Entry::Nav(e) => {
                out.push_str("<entry>");
                push_el(&mut out, "title", &e.title);
                push_el(&mut out, "updated", &format_updated(e.updated));
                push_el(&mut out, "id", &e.id);
                push_el(&mut out, "content", &e.content);
                write_link(&mut out, &e.link);
                out.push_str("</entry>");
            }
            Entry::Acq(e) => {
                out.push_str("<entry>");
                push_el(&mut out, "title", &e.title);
                push_el(&mut out, "updated", &format_updated(e.updated));
                push_el(&mut out, "id", &e.id);
                push_el(&mut out, "content", &e.content);
                for author in &e.authors {
                    out.push_str("<author>");
                    push_el(&mut out, "name", author);
                    out.push_str("</author>");
                }
                for link in &e.links {
                    write_link(&mut out, link);
                }
                out.push_str("</entry>");
            }
        }
    }
    out.push_str("</feed>");
    out
}

fn write_open_search(template: &str) -> String {
    format!(
        "<OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\
         <ShortName>Search</ShortName>\
         <Description>Search for series</Description>\
         <InputEncoding>UTF-8</InputEncoding>\
         <OutputEncoding>UTF-8</OutputEncoding>\
         <Url template=\"{}\" type=\"{TYPE_ACQ}\"/>\
         </OpenSearchDescription>",
        esc_attr(template)
    )
}

fn atom_response(xml: String) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/atom+xml")],
        xml,
    )
        .into_response()
}

// endregion

// region URL building

/// RFC 3986 `pchar` plus `/` (Spring's `UriComponentsBuilder.path`, which preserves slashes).
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        let pchar = c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | ':'
                    | '@'
            );
        if pchar || c == '/' {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// Spring's `UriUtils.encodeQueryParam`: `pchar` minus `&` and `=`.
fn encode_query_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        let pchar = c.is_ascii_alphanumeric()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '$'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | ':'
                    | '@'
                    | '?'
                    | '/'
            );
        if pchar {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `uriBuilder(path)` = `fromCurrentContextPath().pathSegment("opds", "v1.2").path(path)`
fn uri(base: &str, path: &str) -> String {
    let mut url = base.to_string();
    path_segment(&mut url, &["opds", "v1.2"]);
    url.push_str(&encode_path(path));
    url
}

fn with_query(url: &str, params: &[(&str, &str)]) -> String {
    let mut out = url.to_string();
    for (i, (key, value)) in params.iter().enumerate() {
        out.push(if i == 0 { '?' } else { '&' });
        out.push_str(key);
        out.push('=');
        out.push_str(&encode_query_param(value));
    }
    out
}

fn sanitize(file_name: &str) -> String {
    file_name.replace(';', "")
}

fn file_name_of(url: &str) -> &str {
    url.rsplit(['/', '\\']).next().unwrap_or(url)
}

fn extension_of(url: &str) -> &str {
    let name = file_name_of(url);
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

// endregion

// endregion

// region feed builders

fn link_start(base: &str) -> AtomLink {
    AtomLink::nav("start", uri(base, "/catalog"))
}

fn link_page<T>(builder: &str, page: &DtoPage<T>, pageable_page: u32, size: u32) -> Vec<AtomLink> {
    let total_pages = if size == 0 {
        0
    } else {
        (page.total as u64).div_ceil(size as u64) as u32
    };
    let mut links = vec![];
    if pageable_page > 0 {
        links.push(AtomLink::nav(
            "previous",
            with_query(builder, &[("page", &(pageable_page - 1).to_string())]),
        ));
    }
    if pageable_page + 1 < total_pages {
        links.push(AtomLink::nav(
            "next",
            with_query(builder, &[("page", &(pageable_page + 1).to_string())]),
        ));
    }
    links
}

fn page_request(page: u32, size: u32, sort: Vec<DbSortOrder>) -> PageRequest {
    PageRequest {
        page,
        size,
        unpaged: false,
        sort,
    }
}

fn series_entry_nav(series: &komga_core::dto::series::SeriesDto, base: &str) -> Entry {
    Entry::Nav(EntryNav {
        title: series.metadata.title.clone(),
        updated: series.last_modified,
        id: series.id.clone(),
        content: String::new(),
        link: AtomLink::nav("subsection", uri(base, &format!("/series/{}", series.id))),
    })
}

fn library_entry_nav(library: &Library, base: &str) -> Entry {
    Entry::Nav(EntryNav {
        title: library.name.clone(),
        updated: at_zone(library.last_modified_date),
        id: library.id.clone(),
        content: String::new(),
        link: AtomLink::nav(
            "subsection",
            uri(base, &format!("/libraries/{}", library.id)),
        ),
    })
}

fn collection_entry_nav(
    collection: &komga_core::dto::collection::CollectionDto,
    base: &str,
) -> Entry {
    Entry::Nav(EntryNav {
        title: collection.name.clone(),
        updated: at_zone(collection.last_modified_date),
        id: collection.id.clone(),
        content: String::new(),
        link: AtomLink::nav(
            "subsection",
            uri(base, &format!("/collections/{}", collection.id)),
        ),
    })
}

fn readlist_entry_nav(readlist: &komga_core::dto::readlist::ReadListDto, base: &str) -> Entry {
    Entry::Nav(EntryNav {
        title: readlist.name.clone(),
        updated: at_zone(readlist.last_modified_date),
        id: readlist.id.clone(),
        content: String::new(),
        link: AtomLink::nav(
            "subsection",
            uri(base, &format!("/readlists/{}", readlist.id)),
        ),
    })
}

const PSE_SUPPORTED: [&str; 3] = ["image/jpeg", "image/png", "image/gif"];

fn book_entry_acq(book: &BookDto, media: &Media, prepend: &str, base: &str) -> Entry {
    let profile = container::media_profile(media.media_type.as_deref());
    let media_types: Vec<&str> = match profile {
        Some(MediaProfile::Divina) => media
            .pages
            .iter()
            .map(|p| p.media_type.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        Some(MediaProfile::Pdf) => vec!["image/jpeg"],
        Some(MediaProfile::Epub) if media.epub_divina_compatible => media
            .pages
            .iter()
            .map(|p| p.media_type.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        _ => vec![],
    };

    let (page, read_date) = match &book.read_progress {
        Some(rp) => (Some(rp.page), Some(rp.read_date)),
        None => (None, None),
    };
    let streaming = if media_types.is_empty() {
        None
    } else if media_types.len() == 1 && PSE_SUPPORTED.contains(&media_types[0]) {
        Some(AtomLink::page_streaming(
            media_types[0],
            uri(base, &format!("/books/{}/pages/", book.id)) + "{pageNumber}",
            media.page_count,
            page,
            read_date,
        ))
    } else {
        Some(AtomLink::page_streaming(
            "image/jpeg",
            uri(base, &format!("/books/{}/pages/", book.id)) + "{pageNumber}?convert=jpeg",
            media.page_count,
            page,
            read_date,
        ))
    };

    let mut content = format!("{} - {}", extension_of(&book.url).to_lowercase(), book.size);
    if !book.metadata.summary.is_empty() {
        content.push_str(&format!("\n\n{}", book.metadata.summary));
    }

    let mut links = vec![
        AtomLink::image_thumbnail(
            "image/jpeg",
            uri(base, &format!("/books/{}/thumbnail/small", book.id)),
        ),
        AtomLink::image(
            "image/jpeg",
            uri(base, &format!("/books/{}/thumbnail", book.id)),
        ),
        AtomLink::file_acquisition(
            media.media_type.as_deref(),
            uri(
                base,
                &format!(
                    "/books/{}/file/{}",
                    book.id,
                    sanitize(file_name_of(&book.url))
                ),
            ),
        ),
    ];
    links.extend(streaming);

    Entry::Acq(EntryAcq {
        title: format!("{prepend}{}", book.metadata.title),
        updated: komga_core::time_codec::to_zoned_date_time(book.last_modified),
        id: book.id.clone(),
        content: content.replace('\n', "<br/>"),
        authors: book
            .metadata
            .authors
            .iter()
            .map(|a| a.name.clone())
            .collect(),
        links,
    })
}

fn entries_with_series_title(
    state: &AppState,
    books: &[BookDto],
    base: &str,
) -> Result<Vec<Entry>, ApiError> {
    entries_with_prepend(state, books, base, |book| {
        format!("{} {}: ", book.series_title, book.metadata.number)
    })
}

/// Book entries without the series-title prepend (komga only prepends in the
/// keep-reading/on-deck/latest/readlist feeds, not in the series detail feed).
fn entries_plain(state: &AppState, books: &[BookDto], base: &str) -> Result<Vec<Entry>, ApiError> {
    entries_with_prepend(state, books, base, |_| String::new())
}

fn entries_with_prepend(
    state: &AppState,
    books: &[BookDto],
    base: &str,
    prepend: impl Fn(&BookDto) -> String,
) -> Result<Vec<Entry>, ApiError> {
    let dao = MediaDao::new(state.db.clone());
    books
        .iter()
        .map(|book| {
            let media = dao
                .find_by_id(&book.id)?
                .ok_or_else(|| ApiError::Internal(format!("no media for book {}", book.id)))?;
            Ok(book_entry_acq(book, &media, &prepend(book), base))
        })
        .collect()
}

// endregion

// region endpoints

async fn get_catalog(
    State(state): State<AppState>,
    _auth: RequireAuth,
    headers: HeaderMap,
) -> Response {
    let base = base_url_from_headers(&headers, &state.config);
    catalog_feed(&base)
}

/// Catalog entries; takes the request base (tests call it directly).
fn catalog_feed(base: &str) -> Response {
    let now = now_zoned();
    let entries = [
        (
            "Keep Reading",
            "keepReading",
            "Continue reading your in progress books",
            "/keep-reading",
        ),
        ("On Deck", "ondeck", "Browse what to read next", "/ondeck"),
        ("All series", "allSeries", "Browse by series", "/series"),
        (
            "Latest series",
            "latestSeries",
            "Browse latest series",
            "/series/latest",
        ),
        (
            "Latest books",
            "latestBooks",
            "Browse latest books",
            "/books/latest",
        ),
        (
            "All libraries",
            "allLibraries",
            "Browse by library",
            "/libraries",
        ),
        (
            "All collections",
            "allCollections",
            "Browse by collection",
            "/collections",
        ),
        (
            "All read lists",
            "allReadLists",
            "Browse by read lists",
            "/readlists",
        ),
        (
            "Smart lists",
            "allSmartLists",
            "Browse by smart lists",
            "/smart-lists",
        ),
        (
            "All publishers",
            "allPublishers",
            "Browse by publishers",
            "/publishers",
        ),
    ]
    .into_iter()
    .map(|(title, id, content, path)| {
        Entry::Nav(EntryNav {
            title: title.to_string(),
            updated: now,
            id: id.to_string(),
            content: content.to_string(),
            link: AtomLink::nav("subsection", uri(base, path)),
        })
    })
    .collect();

    let feed = Feed {
        id: "root".to_string(),
        title: "Komga OPDS catalog".to_string(),
        updated: now,
        entries,
        links: vec![
            AtomLink::nav("self", uri(base, "/catalog")),
            link_start(base),
            AtomLink::plain(
                "application/opensearchdescription+xml",
                "search",
                uri(base, "/search"),
            ),
            AtomLink::plain("application/opds+json", "alternate", {
                let mut url = base.to_string();
                path_segment(&mut url, &["opds", "v2", "catalog"]);
                url
            }),
        ],
    };
    atom_response(write_feed(&feed))
}

async fn get_search(
    State(state): State<AppState>,
    _auth: RequireAuth,
    headers: HeaderMap,
) -> Response {
    let base = base_url_from_headers(&headers, &state.config);
    atom_response(write_open_search(
        &(uri(&base, "/series") + "?search={searchTerms}"),
    ))
}

async fn get_on_deck(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let book_page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all_on_deck(
            &user.id,
            user.get_authorized_library_ids(None).as_ref(),
            &user.restrictions,
            &page_request(query.pageable.page, query.pageable.size, vec![]),
        )?;

    let entries = entries_with_series_title(&state, &book_page.items, &base)?;
    let builder = uri(&base, "/ondeck");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &book_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "ondeck".to_string(),
        title: "On Deck".to_string(),
        updated: now_zoned(),
        entries,
        links,
    })))
}

async fn get_keep_reading(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let book_page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &BookSearch {
                condition: Some(SearchConditionBook::AllOf {
                    conditions: vec![
                        SearchConditionBook::ReadStatus {
                            operator: Equality::Is {
                                value: ReadStatus::InProgress,
                            },
                        },
                        SearchConditionBook::MediaStatus {
                            operator: Equality::Is {
                                value: MediaStatus::Ready,
                            },
                        },
                        SearchConditionBook::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: "readProgress.readDate".into(),
                    descending: true,
                }],
            ),
        )?;

    let entries = entries_with_series_title(&state, &book_page.items, &base)?;
    let builder = uri(&base, "/keep-reading");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &book_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "keepReading".to_string(),
        title: "Keep Reading".to_string(),
        updated: now_zoned(),
        entries,
        links,
    })))
}

async fn get_all_series(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let search_term = query.params.first("search").map(str::to_string);
    let publishers = query.params.all("publisher");

    let mut conditions = vec![];
    if let Some(term) = &search_term {
        conditions.push(SearchConditionSeries::Title {
            title: StringOp::Contains {
                value: term.clone(),
            },
        });
    }
    if !publishers.is_empty() {
        conditions.push(SearchConditionSeries::AnyOf {
            conditions: publishers
                .iter()
                .map(|p| SearchConditionSeries::Publisher {
                    publisher: Equality::Is { value: p.clone() },
                })
                .collect(),
        });
    }
    conditions.push(SearchConditionSeries::Deleted {
        deleted: BooleanOp::IsFalse,
    });

    let sort = if search_term.as_deref().is_some_and(|s| !s.trim().is_empty()) {
        vec![DbSortOrder {
            property: "relevance".into(),
            descending: false,
        }]
    } else {
        vec![DbSortOrder {
            property: "metadata.titleSort".into(),
            descending: false,
        }]
    };

    let series_page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &SeriesSearch {
                condition: Some(SearchConditionSeries::AllOf { conditions }),
                full_text_search: None,
            },
            None,
            &SearchContext::of_user(user),
            &page_request(query.pageable.page, query.pageable.size, sort),
        )?;

    let mut builder_params: Vec<(&str, &str)> = vec![];
    if let Some(term) = &search_term {
        builder_params.push(("search", term));
    }
    for publisher in publishers {
        builder_params.push(("publisher", publisher));
    }
    let builder = with_query(&uri(&base, "/series"), &builder_params);
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &series_page,
        query.pageable.page,
        query.pageable.size,
    ));

    let title = match &search_term {
        Some(term) if !term.trim().is_empty() => format!("Series search for: {term}"),
        _ => "All series".to_string(),
    };

    Ok(atom_response(write_feed(&Feed {
        id: "allSeries".to_string(),
        title,
        updated: now_zoned(),
        entries: series_page
            .items
            .iter()
            .map(|s| series_entry_nav(s, &base))
            .collect(),
        links,
    })))
}

async fn get_latest_series(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let series_page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &SeriesSearch {
                condition: Some(SearchConditionSeries::Deleted {
                    deleted: BooleanOp::IsFalse,
                }),
                full_text_search: None,
            },
            None,
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: "lastModified".into(),
                    descending: true,
                }],
            ),
        )?;

    let builder = uri(&base, "/series/latest");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &series_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "latestSeries".to_string(),
        title: "Latest series".to_string(),
        updated: now_zoned(),
        entries: series_page
            .items
            .iter()
            .map(|s| series_entry_nav(s, &base))
            .collect(),
        links,
    })))
}

async fn get_latest_books(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let book_page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &BookSearch {
                condition: Some(SearchConditionBook::AllOf {
                    conditions: vec![
                        SearchConditionBook::MediaStatus {
                            operator: Equality::Is {
                                value: MediaStatus::Ready,
                            },
                        },
                        SearchConditionBook::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: "createdDate".into(),
                    descending: true,
                }],
            ),
        )?;

    let entries = entries_with_series_title(&state, &book_page.items, &base)?;
    let builder = uri(&base, "/books/latest");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &book_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "latestBooks".to_string(),
        title: "Latest books".to_string(),
        updated: now_zoned(),
        entries,
        links,
    })))
}

async fn get_libraries(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let dao = LibraryDao::new(state.db.clone());
    let libraries = if user.can_access_all_libraries() {
        dao.find_all()?
    } else {
        dao.find_all()?
            .into_iter()
            .filter(|l| user.shared_libraries_ids.contains(&l.id))
            .collect()
    };

    Ok(atom_response(write_feed(&Feed {
        id: "allLibraries".to_string(),
        title: "All libraries".to_string(),
        updated: now_zoned(),
        entries: libraries
            .iter()
            .map(|l| library_entry_nav(l, &base))
            .collect(),
        links: vec![
            AtomLink::nav("self", uri(&base, "/libraries")),
            link_start(&base),
        ],
    })))
}

async fn get_collections(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let collections = CollectionDtoDao::new(state.db.clone()).find_all(
        user.get_authorized_library_ids(None).as_ref(),
        user.get_authorized_library_ids(None).as_ref(),
        None,
        &page_request(
            query.pageable.page,
            query.pageable.size,
            vec![DbSortOrder {
                property: "name".into(),
                descending: false,
            }],
        ),
        &user.restrictions,
    )?;

    let builder = uri(&base, "/collections");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &collections,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "allCollections".to_string(),
        title: "All collections".to_string(),
        updated: now_zoned(),
        entries: collections
            .items
            .iter()
            .map(|c| collection_entry_nav(c, &base))
            .collect(),
        links,
    })))
}

async fn get_readlists(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let readlists = ReadListDtoDao::new(state.db.clone()).find_all(
        user.get_authorized_library_ids(None).as_ref(),
        user.get_authorized_library_ids(None).as_ref(),
        None,
        &page_request(
            query.pageable.page,
            query.pageable.size,
            vec![DbSortOrder {
                property: "name".into(),
                descending: false,
            }],
        ),
        &user.restrictions,
    )?;

    let builder = uri(&base, "/readlists");
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &readlists,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "allReadLists".to_string(),
        title: "All read lists".to_string(),
        updated: now_zoned(),
        entries: readlists
            .items
            .iter()
            .map(|r| readlist_entry_nav(r, &base))
            .collect(),
        links,
    })))
}

async fn get_smart_lists(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let lists = crate::service::smart_list::find_visible_lists(&state, user)?;

    // in-memory paging like the list API: the count of lists stays small
    let total = lists.len() as i64;
    let size = query.pageable.size.max(1) as usize;
    let offset = (query.pageable.page as usize) * size;
    let items: Vec<komga_core::model::smart_list::SmartList> =
        lists.into_iter().skip(offset).take(size).collect();

    let builder = uri(&base, "/smart-lists");
    let page = DtoPage {
        items: items.clone(),
        total,
        sorted: true,
    };
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "allSmartLists".to_string(),
        title: "Smart lists".to_string(),
        updated: now_zoned(),
        entries: items
            .iter()
            .map(|l| smart_list_entry_nav(l, &base))
            .collect(),
        links,
    })))
}

fn smart_list_entry_nav(
    smart_list: &komga_core::model::smart_list::SmartList,
    base: &str,
) -> Entry {
    Entry::Nav(EntryNav {
        title: smart_list.name.clone(),
        updated: at_zone(smart_list.last_modified_date),
        id: smart_list.id.clone(),
        content: format!(
            "{} smart list{}",
            smart_list.target.as_str(),
            if smart_list.summary.is_empty() {
                String::new()
            } else {
                format!(" — {}", smart_list.summary)
            },
        ),
        link: AtomLink::nav(
            "subsection",
            uri(base, &format!("/smart-lists/{}", smart_list.id)),
        ),
    })
}

async fn get_one_smart_list(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let smart_list = crate::service::smart_list::find_visible(&state, user, &id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let ctx = SearchContext::of_user(user);
    let searcher = Some(crate::search_index::searcher(&state));
    // the sort property is target-specific: the series DAO drops unknown keys, which
    // would leave SERIES feeds unordered and pagination unstable
    let page = page_request(
        query.pageable.page,
        query.pageable.size,
        vec![DbSortOrder {
            property: crate::service::smart_list::opds_sort_property(smart_list.target).into(),
            descending: false,
        }],
    );

    let (entries, total) = match smart_list.target {
        komga_core::model::smart_list::SmartListTarget::Book => {
            let search = crate::service::smart_list::opds_book_search(&smart_list)?;
            let books_page = BookDtoDao::new(state.db.clone())
                .with_searcher(searcher)
                .find_all(&search, &ctx, &page)?;
            let total = books_page.total;
            (
                entries_with_series_title(&state, &books_page.items, &base)?,
                total,
            )
        }
        komga_core::model::smart_list::SmartListTarget::Series => {
            let search = crate::service::smart_list::opds_series_search(&smart_list)?;
            let series_page = SeriesDtoDao::new(state.db.clone())
                .with_searcher(searcher)
                .find_all(&search, None, &ctx, &page)?;
            let total = series_page.total;
            (
                series_page
                    .items
                    .iter()
                    .map(|s| series_entry_nav(s, &base))
                    .collect(),
                total,
            )
        }
    };

    let builder = uri(&base, &format!("/smart-lists/{id}"));
    // link_page only reads the total
    let page_info = DtoPage::<()> {
        items: vec![],
        total,
        sorted: true,
    };
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &page_info,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: smart_list.id.clone(),
        title: smart_list.name.clone(),
        updated: at_zone(smart_list.last_modified_date),
        entries,
        links,
    })))
}

async fn get_publishers(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let publishers = ReferentialDao::new(state.db.clone())
        .find_all_publishers(user.get_authorized_library_ids(None).as_ref())?;

    // the Kotlin side pages a distinct-publisher query; the dao already returns the full
    // distinct list in the same order, so paging in memory is equivalent
    let total = publishers.len() as i64;
    let size = query.pageable.size.max(1) as usize;
    let offset = (query.pageable.page as usize) * size;
    let items: Vec<String> = publishers.into_iter().skip(offset).take(size).collect();

    let builder = uri(&base, "/publishers");
    let page = DtoPage {
        items: items.clone(),
        total,
        sorted: true,
    };
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: "allPublishers".to_string(),
        title: "All publishers".to_string(),
        updated: now_zoned(),
        entries: items
            .iter()
            .map(|publisher| {
                Entry::Nav(EntryNav {
                    title: publisher.clone(),
                    updated: now_zoned(),
                    id: format!("publisher:{}", encode_query_param(publisher)),
                    content: String::new(),
                    link: AtomLink::nav(
                        "subsection",
                        with_query(&uri(&base, "/series"), &[("publisher", publisher)]),
                    ),
                })
            })
            .collect(),
        links,
    })))
}

async fn get_one_series(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let series = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_by_id(&id, &user.id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    restriction::check_series_dto(user, &series)?;

    let books_page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &BookSearch {
                condition: Some(SearchConditionBook::AllOf {
                    conditions: vec![
                        SearchConditionBook::SeriesId {
                            operator: Equality::Is { value: id.clone() },
                        },
                        SearchConditionBook::MediaStatus {
                            operator: Equality::Is {
                                value: MediaStatus::Ready,
                            },
                        },
                        SearchConditionBook::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: "metadata.numberSort".into(),
                    descending: false,
                }],
            ),
        )?;

    let entries = entries_plain(&state, &books_page.items, &base)?;
    let builder = uri(&base, &format!("/series/{id}"));
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &books_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: series.id.clone(),
        title: series.metadata.title.clone(),
        updated: komga_core::time_codec::to_zoned_date_time(series.last_modified),
        entries,
        links,
    })))
}

async fn get_one_library(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let library = LibraryDao::new(state.db.clone())
        .find_by_id(&id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    if !user.can_access_library(&library.id) {
        return Err(ApiError::forbidden(""));
    }

    let entries_page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &SeriesSearch {
                condition: Some(SearchConditionSeries::AllOf {
                    conditions: vec![
                        SearchConditionSeries::LibraryId {
                            operator: Equality::Is { value: id.clone() },
                        },
                        SearchConditionSeries::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            None,
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: "metadata.titleSort".into(),
                    descending: false,
                }],
            ),
        )?;

    let builder = uri(&base, &format!("/libraries/{id}"));
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &entries_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: library.id.clone(),
        title: library.name.clone(),
        updated: at_zone(library.last_modified_date),
        entries: entries_page
            .items
            .iter()
            .map(|s| series_entry_nav(s, &base))
            .collect(),
        links,
    })))
}

async fn get_one_collection(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let collection = CollectionDtoDao::new(state.db.clone())
        .find_by_id(
            &id,
            user.get_authorized_library_ids(None).as_ref(),
            &user.restrictions,
        )?
        .ok_or_else(|| ApiError::not_found(""))?;

    let sort = if collection.ordered {
        "collection.number"
    } else {
        "metadata.titleSort"
    };
    let entries_page = SeriesDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &SeriesSearch {
                condition: Some(SearchConditionSeries::AllOf {
                    conditions: vec![
                        SearchConditionSeries::CollectionId {
                            operator: Equality::Is {
                                value: collection.id.clone(),
                            },
                        },
                        SearchConditionSeries::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            None,
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: sort.into(),
                    descending: false,
                }],
            ),
        )?;

    let builder = uri(&base, &format!("/collections/{id}"));
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &entries_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: collection.id.clone(),
        title: collection.name.clone(),
        updated: at_zone(collection.last_modified_date),
        entries: entries_page
            .items
            .iter()
            .map(|s| series_entry_nav(s, &base))
            .collect(),
        links,
    })))
}

async fn get_one_readlist(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    let base = base_url_from_headers(&headers, &state.config);
    let user = &auth.0.user;
    let readlist = ReadListDtoDao::new(state.db.clone())
        .find_by_id(
            &id,
            user.get_authorized_library_ids(None).as_ref(),
            &user.restrictions,
        )?
        .ok_or_else(|| ApiError::not_found(""))?;

    let sort = if readlist.ordered {
        "readList.number"
    } else {
        "metadata.releaseDate"
    };
    let books_page = BookDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            &BookSearch {
                condition: Some(SearchConditionBook::AllOf {
                    conditions: vec![
                        SearchConditionBook::ReadListId {
                            operator: Equality::Is {
                                value: readlist.id.clone(),
                            },
                        },
                        SearchConditionBook::MediaStatus {
                            operator: Equality::Is {
                                value: MediaStatus::Ready,
                            },
                        },
                        SearchConditionBook::Deleted {
                            deleted: BooleanOp::IsFalse,
                        },
                    ],
                }),
                full_text_search: None,
            },
            &SearchContext::of_user(user),
            &page_request(
                query.pageable.page,
                query.pageable.size,
                vec![DbSortOrder {
                    property: sort.into(),
                    descending: false,
                }],
            ),
        )?;

    let entries = entries_with_series_title(&state, &books_page.items, &base)?;
    let builder = uri(&base, &format!("/readlists/{id}"));
    let mut links = vec![AtomLink::nav("self", builder.clone()), link_start(&base)];
    links.extend(link_page(
        &builder,
        &books_page,
        query.pageable.page,
        query.pageable.size,
    ));

    Ok(atom_response(write_feed(&Feed {
        id: readlist.id.clone(),
        title: readlist.name.clone(),
        updated: at_zone(readlist.last_modified_date),
        entries,
        links,
    })))
}

async fn get_book_thumbnail_small(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(book_id): Path<String>,
) -> Result<Response, ApiError> {
    restriction::check_book_by_id(&state, &auth.0.user, &book_id)?;
    let thumbnail = crate::service::book::get_thumbnail(&state, &book_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let resize_to = if thumbnail.type_ == ThumbnailType::Generated {
        None
    } else {
        Some(state.settings.get().thumbnail_size.max_edge())
    };
    let content = crate::service::book::get_thumbnail_bytes(&state, &book_id, resize_to)?
        .ok_or_else(|| ApiError::not_found(""))?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
        content.bytes,
    )
        .into_response())
}

async fn get_book_thumbnail(
    State(state): State<AppState>,
    auth: RequireAuth,
    Path(book_id): Path<String>,
) -> Result<Response, ApiError> {
    restriction::check_book_by_id(&state, &auth.0.user, &book_id)?;
    let poster = crate::service::book::get_thumbnail_bytes_original(&state, &book_id)?
        .ok_or_else(|| ApiError::not_found(""))?;
    let bytes = if poster.media_type != ImageType::Jpeg.media_type() {
        image::convert(&poster.bytes, ImageType::Jpeg)
            .map_err(|e| ApiError::Internal(format!("convert thumbnail to jpeg: {e}")))?
    } else {
        poster.bytes
    };
    Ok(([(axum::http::header::CONTENT_TYPE, "image/jpeg")], bytes).into_response())
}

async fn get_book_page_opds(
    State(state): State<AppState>,
    auth: RequireAuth,
    headers: HeaderMap,
    Path((book_id, page_number)): Path<(String, i32)>,
    query: QueryPageable,
) -> Result<Response, ApiError> {
    auth.0.require_role(UserRole::PageStreaming)?;
    // OPDS-PSE pages are 0-based
    super::books::get_page_internal(
        &state,
        &auth.0.user,
        &headers,
        &book_id,
        page_number + 1,
        super::books::PageOptions {
            convert: query.params.first("convert"),
            resize_to: None,
            accept: None,
            with_disposition: true,
        },
    )
    .await
}

// endregion

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use crate::config::ServerConfig;
    use crate::http;
    use crate::state::test_kmrs_db;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use komga_core::model::read_progress::ReadProgress;
    use komga_core::model::thumbnail::ThumbnailBook;
    use komga_core::model::user::{ApiKey, ContentRestrictions};
    use komga_core::time_codec::now_utc;
    use komga_db::dao::read_progress::ReadProgressDao;
    use komga_db::dao::thumbnail::ThumbnailBookDao;
    use komga_db::dao::user::UserDao;
    use komga_db::pool::{Database, DatabaseConfig, JournalMode};
    use komga_db::{Migrator, Placeholders};
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let db = Database::open_in_memory(true).unwrap();
        let migrations = komga_db::main_migrations();
        Migrator::new(&migrations, Placeholders::default())
            .migrate(&db.rw().unwrap())
            .unwrap();
        let tasks_db = Database::open_in_memory(false).unwrap();
        let tasks_migrations = komga_db::tasks_migrations();
        Migrator::new(&tasks_migrations, Placeholders::default())
            .migrate(&tasks_db.rw().unwrap())
            .unwrap();
        let db_config = |register_udfs| DatabaseConfig {
            file: std::env::temp_dir(),
            register_udfs,
            journal_mode: JournalMode::Wal,
            ..Default::default()
        };
        let config = ServerConfig {
            config_dir: std::env::temp_dir(),
            lucene_dir: std::env::temp_dir(),
            fonts_dir: std::env::temp_dir(),
            port: 0,
            database: db_config(true),
            tasks_db: db_config(false),
            kmrs_db: db_config(false),
            sessions_db: db_config(false),
            jdbc_session_enabled: false,
            session_timeout: std::time::Duration::from_secs(3600),
            cors_allowed_origins: vec![],
            page_hashing: 3,
            epub_divina_letter_count_threshold: 15,
            kobo_sync_item_limit: 100,
            kepubify_path: None,
            server_context_path: None,
            webhooks: Default::default(),
            migration_placeholders: Default::default(),
            oauth2: Default::default(),

            komf_url: None,
            komf_base_url: None,
            komf_auth_key: None,
            history_retention_days: 180,
            update_check_enabled: true,
            log_level: None,
            sort_locale: None,
            thumbnail_storage: Default::default(),
            thumbnail_deep_etag: true,
        };
        crate::state::test_app_state(config, db, tasks_db, test_kmrs_db()).0
    }

    fn test_app(state: &AppState) -> axum::Router {
        router()
            .layer(axum::middleware::from_fn(
                http::error_path::error_path_middleware,
            ))
            .layer(axum::middleware::from_fn(http::etag::etag_middleware))
            .layer(axum::middleware::from_fn(
                http::cache::cache_control_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                auth::auth_middleware,
            ))
            .with_state(state.clone())
    }

    const ADMIN_KEY: &str = "admin-key";

    fn seed_user(
        state: &AppState,
        email: &str,
        roles: &[UserRole],
        key: &str,
    ) -> komga_core::model::user::KomgaUser {
        let user = komga_core::model::user::KomgaUser {
            id: String::new(),
            email: email.into(),
            password: "x".into(),
            roles: roles.iter().cloned().collect(),
            shared_libraries_ids: BTreeSet::new(),
            shared_all_libraries: true,
            restrictions: ContentRestrictions::default(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        };
        let dao = UserDao::new(state.db.clone());
        let id = dao.insert(&user).unwrap();
        dao.insert_api_key(&ApiKey {
            id: String::new(),
            user_id: id.clone(),
            key: auth::sha512_hex(key),
            comment: "test".into(),
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();
        dao.find_by_id(&id).unwrap().unwrap()
    }

    fn admin(state: &AppState) -> komga_core::model::user::KomgaUser {
        seed_user(state, "admin@komga.org", &[UserRole::Admin], ADMIN_KEY)
    }

    fn seed_library(state: &AppState, id: &str, name: &str) {
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "INSERT INTO LIBRARY (ID, NAME, ROOT) VALUES (?, ?, ?)",
                (id, name, format!("file:/data/{name}/")),
            )
            .unwrap();
    }

    fn seed_series(state: &AppState, id: &str, library_id: &str, name: &str) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID) \
             VALUES (?, ?, ?, '2020-01-01 00:00:00.0', ?)",
            (id, name, format!("file:/data/{name}/"), library_id),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO SERIES_METADATA (SERIES_ID, STATUS, TITLE, TITLE_SORT) VALUES (?, 'ONGOING', ?, ?)",
            (id, name, name),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO BOOK_METADATA_AGGREGATION (SERIES_ID) VALUES (?)",
            [id],
        )
        .unwrap();
    }

    fn seed_book(state: &AppState, id: &str, series_id: &str, library_id: &str, name: &str) {
        seed_book_pages(state, id, series_id, library_id, name, 1.0, &["image/png"]);
    }

    fn seed_book_pages(
        state: &AppState,
        id: &str,
        series_id: &str,
        library_id: &str,
        name: &str,
        number_sort: f32,
        page_types: &[&str],
    ) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
             VALUES (?, ?, ?, '2020-01-01 00:00:00.0', ?, ?)",
            (
                id,
                name,
                format!("file:/data/{name}.cbz"),
                series_id,
                library_id,
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO MEDIA (BOOK_ID, STATUS, MEDIA_TYPE, PAGE_COUNT) VALUES (?, 'READY', 'application/zip', ?)",
            (id, page_types.len() as i64),
        )
        .unwrap();
        for (i, media_type) in page_types.iter().enumerate() {
            conn.execute(
                "INSERT INTO MEDIA_PAGE (BOOK_ID, FILE_NAME, MEDIA_TYPE, NUMBER, FILE_HASH) VALUES (?, ?, ?, ?, '')",
                (id, format!("page-{}.bin", i + 1), media_type, i as i64),
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO BOOK_METADATA (BOOK_ID, NUMBER, NUMBER_SORT, TITLE) VALUES (?, ?, ?, ?)",
            (id, number_sort as i64, number_sort, name),
        )
        .unwrap();
        conn.execute(
            "UPDATE SERIES SET BOOK_COUNT = (SELECT COUNT(*) FROM BOOK WHERE SERIES_ID = ?) WHERE ID = ?",
            (series_id, series_id),
        )
        .unwrap();
    }

    fn seed_progress(state: &AppState, book_id: &str, user_id: &str, page: i32, completed: bool) {
        ReadProgressDao::new(state.db.clone())
            .insert_or_update(&ReadProgress {
                book_id: book_id.into(),
                user_id: user_id.into(),
                page,
                completed,
                read_date: now_utc(),
                device_id: String::new(),
                device_name: String::new(),
                locator: None,
                created_date: now_utc(),
                last_modified_date: now_utc(),
            })
            .unwrap();
    }

    fn seed_collection(state: &AppState, id: &str, name: &str, series_ids: &[&str]) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO COLLECTION (ID, NAME, SERIES_COUNT) VALUES (?, ?, ?)",
            (id, name, series_ids.len() as i64),
        )
        .unwrap();
        for (i, sid) in series_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO COLLECTION_SERIES (COLLECTION_ID, SERIES_ID, NUMBER) VALUES (?, ?, ?)",
                (id, sid, i as i64),
            )
            .unwrap();
        }
    }

    fn seed_readlist(state: &AppState, id: &str, name: &str, book_ids: &[&str]) {
        let conn = state.db.rw().unwrap();
        conn.execute(
            "INSERT INTO READLIST (ID, NAME, ORDERED, BOOK_COUNT) VALUES (?, ?, 1, ?)",
            (id, name, book_ids.len() as i64),
        )
        .unwrap();
        for (i, bid) in book_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) VALUES (?, ?, ?)",
                (id, bid, i as i64),
            )
            .unwrap();
        }
    }

    fn seed_publisher(state: &AppState, series_id: &str, publisher: &str) {
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "UPDATE SERIES_METADATA SET PUBLISHER = ? WHERE SERIES_ID = ?",
                (publisher, series_id),
            )
            .unwrap();
    }

    fn authed(path: &str, key: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header("X-API-Key", key)
            .body(Body::empty())
            .unwrap()
    }

    async fn body_string(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn catalog_feed_xml_shape() {
        let response = catalog_feed("http://localhost:25600");
        let xml = body_string(response).await;

        // no XML declaration; navigation-only feed has no pse declaration
        assert!(xml.starts_with("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
        assert!(!xml.contains("xmlns:pse"));
        assert!(xml.contains("<id>root</id>"));
        assert!(xml.contains("<title>Komga OPDS catalog</title>"));
        assert!(xml.contains(
            "<author><name>Komga</name><uri>https://github.com/gotson/komga</uri></author>"
        ));
        assert!(xml.contains("<link type=\"application/atom+xml;profile=opds-catalog;kind=navigation\" rel=\"self\" href=\"http://localhost:25600/opds/v1.2/catalog\"/>"));
        assert!(xml.contains("<link type=\"application/atom+xml;profile=opds-catalog;kind=navigation\" rel=\"start\" href=\"http://localhost:25600/opds/v1.2/catalog\"/>"));
        assert!(xml.contains("<link type=\"application/opensearchdescription+xml\" rel=\"search\" href=\"http://localhost:25600/opds/v1.2/search\"/>"));
        assert!(xml.contains("<link type=\"application/opds+json\" rel=\"alternate\" href=\"http://localhost:25600/opds/v2/catalog\"/>"));
        // 9 komga sections + the kmrs smart lists extension
        assert_eq!(xml.matches("<entry>").count(), 10);
        assert!(xml.contains("<id>keepReading</id>"));
        assert!(xml.contains("<id>allPublishers</id>"));
        assert!(xml.contains("<id>allSmartLists</id>"));
        assert!(xml.contains("<content>Continue reading your in progress books</content>"));

        // `ZonedDateTime.now()` renders in the system zone
        let offset = komga_core::time_codec::system_offset_at(komga_core::time_codec::now_utc());
        let offset_str = if offset.is_utc() {
            "Z".to_string()
        } else {
            let total = offset.whole_seconds();
            let sign = if total < 0 { '-' } else { '+' };
            let abs = total.unsigned_abs();
            format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
        };
        let updated = xml
            .split("<updated>")
            .nth(1)
            .unwrap()
            .split("</updated>")
            .next()
            .unwrap();
        assert!(
            updated.ends_with(&offset_str),
            "updated {updated} should end with system offset {offset_str}"
        );
    }

    #[tokio::test]
    async fn open_search_xml_shape() {
        let response = atom_response(write_open_search(
            "http://localhost:25600/opds/v1.2/series?search={searchTerms}",
        ));
        let xml = body_string(response).await;

        assert!(xml
            .starts_with("<OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">"));
        assert!(!xml.contains("xmlns:pse"));
        assert!(xml.contains("<ShortName>Search</ShortName>"));
        assert!(xml.contains("<Description>Search for series</Description>"));
        assert!(xml.contains("<InputEncoding>UTF-8</InputEncoding>"));
        assert!(xml.contains("<OutputEncoding>UTF-8</OutputEncoding>"));
        assert!(xml.contains("<Url template=\"http://localhost:25600/opds/v1.2/series?search={searchTerms}\" type=\"application/atom+xml;profile=opds-catalog;kind=acquisition\"/>"));
    }

    #[tokio::test]
    async fn catalog_endpoint() {
        let state = test_state();
        admin(&state);
        let app = test_app(&state);
        let response = app
            .oneshot(authed("/opds/v1.2/catalog", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/atom+xml"
        );
        let xml = body_string(response).await;
        assert!(xml.contains("<title>Komga OPDS catalog</title>"));
    }

    #[tokio::test]
    async fn series_feed_pagination_and_filters() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "Library One");
        seed_library(&state, "lib2", "Library Two");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_series(&state, "s3", "lib2", "Bleach");
        seed_publisher(&state, "s1", "Hakusensha");
        seed_publisher(&state, "s2", "Shueisha");
        let app = test_app(&state);

        // unpaged: all three
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/series", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 3);
        assert!(xml.contains("<title>All series</title>"));
        assert!(!xml.contains("rel=\"next\""));
        assert!(!xml.contains("rel=\"previous\""));

        // page 0 of 2: next present, previous absent
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/series?page=0&size=2", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 2);
        assert!(xml.contains("rel=\"next\" href=\"http://localhost/opds/v1.2/series?page=1\""));
        assert!(!xml.contains("rel=\"previous\""));

        // page 1 of 2: previous present, next absent
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/series?page=1&size=2", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 1);
        assert!(xml.contains("rel=\"previous\" href=\"http://localhost/opds/v1.2/series?page=0\""));
        assert!(!xml.contains("rel=\"next\""));

        // search: title changes and the search param is carried into self/next links
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/series?search=ber", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert!(xml.contains("<title>Series search for: ber</title>"));
        assert_eq!(xml.matches("<entry>").count(), 1);
        assert!(xml.contains("<id>s1</id>"));

        // publisher filter
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/series?publisher=Shueisha", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 1);
        assert!(xml.contains("<id>s2</id>"));
    }

    #[tokio::test]
    async fn one_series_feed_page_streaming() {
        let state = test_state();
        let user = admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        // all pages the same supported type: direct media type, no convert
        seed_book_pages(
            &state,
            "b1",
            "s1",
            "lib1",
            "Berserk v01",
            1.0,
            &["image/png", "image/png"],
        );
        // mixed page types: jpeg convert branch
        seed_book_pages(
            &state,
            "b2",
            "s1",
            "lib1",
            "Berserk v02",
            2.0,
            &["image/png", "image/gif"],
        );
        seed_progress(&state, "b1", &user.id, 1, false);
        let app = test_app(&state);

        let response = app
            .oneshot(authed("/opds/v1.2/series/s1", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;

        // the pse namespace is declared inline on each page-streaming link, not on the root
        assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));

        // the series detail feed has no series-title prepend (only latest/keep-reading/readlist feeds do)
        assert!(xml.contains("<title>Berserk v01</title>"));
        assert!(xml.contains("<title>Berserk v02</title>"));

        // b1: same-type pages stream as-is, with progress attributes
        assert!(xml.contains("<link href=\"http://localhost/opds/v1.2/books/b1/pages/{pageNumber}\" xmlns:pse=\"http://vaemendis.net/opds-pse/ns\" pse:count=\"2\" pse:lastRead=\"1\" pse:lastReadDate=\""));
        // b2: mixed types fall back to jpeg convert
        assert!(xml.contains("<link href=\"http://localhost/opds/v1.2/books/b2/pages/{pageNumber}?convert=jpeg\" xmlns:pse=\"http://vaemendis.net/opds-pse/ns\" pse:count=\"2\" type=\"image/jpeg\" rel=\"http://vaemendis.net/opds-pse/stream\"/>"));

        // feed metadata
        assert!(xml.contains("<id>s1</id>"));
        assert!(xml.contains("<title>Berserk</title>"));
        // acquisition links: thumbnail/small, thumbnail, file acquisition
        assert!(xml.contains("rel=\"http://opds-spec.org/image/thumbnail\" href=\"http://localhost/opds/v1.2/books/b1/thumbnail/small\""));
        assert!(xml.contains("rel=\"http://opds-spec.org/image\" href=\"http://localhost/opds/v1.2/books/b1/thumbnail\""));
        assert!(xml.contains("rel=\"http://opds-spec.org/acquisition\" href=\"http://localhost/opds/v1.2/books/b1/file/Berserk%20v01.cbz\""));
        // content includes extension and size
        assert!(xml.contains("cbz - 0 B"));
    }

    #[tokio::test]
    async fn publishers_feed() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_publisher(&state, "s1", "Hakusensha");
        seed_publisher(&state, "s2", "Dark Horse");
        let app = test_app(&state);

        let response = app
            .oneshot(authed("/opds/v1.2/publishers", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 2);
        assert!(xml.contains("<id>publisher:Dark%20Horse</id>"));
        assert!(xml.contains("<id>publisher:Hakusensha</id>"));
        assert!(xml.contains("/opds/v1.2/series?publisher=Dark%20Horse"));
    }

    #[tokio::test]
    async fn publishers_feed_paginates() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        for i in 0..21 {
            let series_id = format!("s{i}");
            let publisher = format!("Publisher {i:02}");
            seed_series(&state, &series_id, "lib1", &publisher);
            seed_publisher(&state, &series_id, &publisher);
        }
        let app = test_app(&state);

        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/publishers", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 20);
        assert!(xml.contains("rel=\"next\" href=\"http://localhost/opds/v1.2/publishers?page=1\""));
        assert!(!xml.contains("rel=\"previous\""));

        let response = app
            .oneshot(authed("/opds/v1.2/publishers?page=1", ADMIN_KEY))
            .await
            .unwrap();
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<entry>").count(), 1);
        assert!(
            xml.contains("rel=\"previous\" href=\"http://localhost/opds/v1.2/publishers?page=0\"")
        );
        assert!(!xml.contains("rel=\"next\""));
    }

    #[tokio::test]
    async fn thumbnail_small_generated_vs_sidecar() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "S");
        seed_book(&state, "b1", "s1", "lib1", "B1");
        seed_book(&state, "b2", "s1", "lib1", "B2");

        // tiny png (48x48 real fixture)
        let png_bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/resources/hashpage/tg.png/1.png"),
        )
        .expect("read png fixture");
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("sidecar.jpg");
        std::fs::write(&sidecar_path, &png_bytes).unwrap();

        let dao = ThumbnailBookDao::new(state.db.clone());
        dao.insert(&ThumbnailBook {
            id: String::new(),
            book_id: "b1".into(),
            thumbnail: Some(png_bytes.clone()),
            url: None,
            selected: true,
            type_: ThumbnailType::Generated,
            media_type: "image/png".into(),
            file_size: png_bytes.len() as i64,
            dimension: komga_core::model::thumbnail::Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();
        dao.insert(&ThumbnailBook {
            id: String::new(),
            book_id: "b2".into(),
            thumbnail: None,
            url: Some(format!("file:{}", sidecar_path.display())),
            selected: true,
            type_: ThumbnailType::Sidecar,
            media_type: "image/png".into(),
            file_size: png_bytes.len() as i64,
            dimension: komga_core::model::thumbnail::Dimension {
                width: 48,
                height: 48,
            },
            created_date: now_utc(),
            last_modified_date: now_utc(),
        })
        .unwrap();

        let app = test_app(&state);

        // generated: original bytes returned untouched
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/books/b1/thumbnail/small", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], &png_bytes[..]);

        // sidecar: resized and re-encoded as jpeg
        let response = app
            .oneshot(authed("/opds/v1.2/books/b2/thumbnail/small", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[0..3], b"\xFF\xD8\xFF");
    }

    #[tokio::test]
    async fn pages_zero_based() {
        let state = test_state();
        admin(&state);
        seed_library(&state, "lib1", "L");
        seed_series(&state, "s1", "lib1", "S");

        // real zip book on disk
        let dir = tempfile::tempdir().unwrap();
        let book_path = dir.path().join("v01.cbz");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/resources/archives/zip.zip"),
            &book_path,
        )
        .expect("copy zip fixture");
        {
            let conn = state.db.rw().unwrap();
            conn.execute(
                "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID) \
                 VALUES ('b1', 'v01', ?, '2020-01-01 00:00:00.0', 's1', 'lib1')",
                [format!("file:{}", book_path.display())],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO MEDIA (BOOK_ID, STATUS, MEDIA_TYPE, PAGE_COUNT) VALUES ('b1', 'READY', 'application/zip', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO MEDIA_PAGE (BOOK_ID, FILE_NAME, MEDIA_TYPE, NUMBER, FILE_HASH) VALUES ('b1', 'komga.png', 'image/png', 0, '')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO BOOK_METADATA (BOOK_ID, NUMBER, NUMBER_SORT, TITLE) VALUES ('b1', 1, 1.0, 'v01')",
                [],
            )
            .unwrap();
        }

        let app = test_app(&state);
        let response = app
            .oneshot(authed("/opds/v1.2/books/b1/pages/0", ADMIN_KEY))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "image/png"
        );
    }

    #[tokio::test]
    async fn unauthenticated_is_401() {
        let state = test_state();
        let app = test_app(&state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/opds/v1.2/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        // OPDS readers discover basic auth from the challenge
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .unwrap(),
            "Basic realm=\"Realm\""
        );
    }

    #[tokio::test]
    async fn smart_lists_feeds_follow_visibility() {
        use komga_core::model::smart_list::{SmartList, SmartListTarget, SmartListVisibility};

        let state = test_state();
        let admin_user = admin(&state);
        let user = seed_user(&state, "user@komga.org", &[], "user-key");
        let user3 = seed_user(&state, "u3@komga.org", &[], "u3-key");
        seed_user(&state, "u4@komga.org", &[], "u4-key");
        seed_library(&state, "lib1", "Library One");
        seed_series(&state, "s1", "lib1", "Berserk");
        seed_series(&state, "s2", "lib1", "Naruto");
        seed_book(&state, "b1", "s1", "lib1", "Berserk v1");
        seed_book(&state, "b2", "s2", "lib1", "Naruto v1");
        state
            .db
            .rw()
            .unwrap()
            .execute(
                "INSERT INTO BOOK_METADATA_TAG (TAG, BOOK_ID) VALUES ('manga', 'b1')",
                [],
            )
            .unwrap();

        let make_list = |owner: &komga_core::model::user::KomgaUser,
                         name: &str,
                         visibility: SmartListVisibility,
                         target: SmartListTarget,
                         search_json: &str| {
            crate::service::smart_list::add_smart_list(
                &state,
                SmartList {
                    id: String::new(),
                    name: name.into(),
                    summary: String::new(),
                    owner_user_id: owner.id.clone(),
                    target,
                    visibility,
                    search_json: search_json.into(),
                    created_date: now_utc(),
                    last_modified_date: now_utc(),
                },
                &[],
            )
            .unwrap()
        };
        let manga_filter = r#"{"condition":{"tag":{"operator":"is","value":"manga"}}}"#;
        let private_list = make_list(
            &user,
            "priv",
            SmartListVisibility::Private,
            SmartListTarget::Book,
            manga_filter,
        );
        let public_list = make_list(
            &admin_user,
            "pub",
            SmartListVisibility::Public,
            SmartListTarget::Book,
            manga_filter,
        );
        let series_list = make_list(
            &user,
            "all-series",
            SmartListVisibility::Private,
            SmartListTarget::Series,
            r#"{"condition":null}"#,
        );

        // release dates make the default ordering observable: b2 (2020) sorts before b1 (2021)
        let conn = state.db.rw().unwrap();
        conn.execute(
            "UPDATE BOOK_METADATA SET RELEASE_DATE = '2021-01-01' WHERE BOOK_ID = 'b1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE BOOK_METADATA SET RELEASE_DATE = '2020-01-01' WHERE BOOK_ID = 'b2'",
            [],
        )
        .unwrap();
        // series ordering reads the aggregated release date, maintained by the
        // metadata aggregation service — seed it directly here
        conn.execute(
            "UPDATE BOOK_METADATA_AGGREGATION SET RELEASE_DATE = '2021-01-01' WHERE SERIES_ID = 's1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE BOOK_METADATA_AGGREGATION SET RELEASE_DATE = '2020-01-01' WHERE SERIES_ID = 's2'",
            [],
        )
        .unwrap();
        drop(conn);

        // an admin-private list: must never show up for the regular user
        let admin_private = make_list(
            &admin_user,
            "admin-secret",
            SmartListVisibility::Private,
            SmartListTarget::Book,
            manga_filter,
        );
        let all_books = make_list(
            &user,
            "all-books",
            SmartListVisibility::Private,
            SmartListTarget::Book,
            r#"{"condition":null}"#,
        );
        // a SHARED list is visible to its share targets through OPDS as well
        let shared_list = make_list(
            &user,
            "shared-with-u3",
            SmartListVisibility::Shared,
            SmartListTarget::Book,
            manga_filter,
        );
        crate::service::smart_list::dao(&state)
            .set_shares(&shared_list.id, std::slice::from_ref(&user3.id))
            .unwrap();
        let app = test_app(&state);

        // the root catalog advertises the smart lists section
        let xml = body_string(
            app.clone()
                .oneshot(authed("/opds/v1.2/catalog", "user-key"))
                .await
                .unwrap(),
        )
        .await;
        assert!(xml.contains("/opds/v1.2/smart-lists"));

        // nav feed: own private list and the admin's public list; the admin's
        // private list is filtered out (real visibility filtering, not just presence)
        let xml = body_string(
            app.clone()
                .oneshot(authed("/opds/v1.2/smart-lists", "user-key"))
                .await
                .unwrap(),
        )
        .await;
        assert!(xml.contains("<title>priv</title>"));
        assert!(xml.contains("<title>pub</title>"));
        assert!(!xml.contains("admin-secret"));

        // BOOK feed serves the matched, ready, non-deleted books
        let xml = body_string(
            app.clone()
                .oneshot(authed(
                    &format!("/opds/v1.2/smart-lists/{}", private_list.id),
                    "user-key",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(xml.contains("Berserk v1"));
        assert!(!xml.contains("Naruto v1"));

        // ordering is pinned: release-date ascending, b2 (2020) before b1 (2021)
        let xml = body_string(
            app.clone()
                .oneshot(authed(
                    &format!("/opds/v1.2/smart-lists/{}", all_books.id),
                    "user-key",
                ))
                .await
                .unwrap(),
        )
        .await;
        let pos_b2 = xml.find("Naruto v1").unwrap();
        let pos_b1 = xml.find("Berserk v1").unwrap();
        assert!(pos_b2 < pos_b1, "expected release-date ascending order");

        // SERIES feed navigates into series, ordered by aggregated release date
        // (s2's book is 2020, s1's is 2021)
        let xml = body_string(
            app.clone()
                .oneshot(authed(
                    &format!("/opds/v1.2/smart-lists/{}", series_list.id),
                    "user-key",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(xml.contains("<title>Berserk</title>"));
        assert!(xml.contains("<title>Naruto</title>"));
        let pos_s2 = xml.find("<title>Naruto</title>").unwrap();
        let pos_s1 = xml.find("<title>Berserk</title>").unwrap();
        assert!(pos_s2 < pos_s1, "expected series ordered by release date");

        // unknown lists 404; admins may open any list; other users' private lists 404
        let response = app
            .clone()
            .oneshot(authed("/opds/v1.2/smart-lists/nope", "user-key"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .clone()
            .oneshot(authed(
                &format!("/opds/v1.2/smart-lists/{}", private_list.id),
                ADMIN_KEY,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(authed(
                &format!("/opds/v1.2/smart-lists/{}", admin_private.id),
                "user-key",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // the share target sees the list in the nav feed and may open it;
        // a user outside the share scope gets neither
        let xml = body_string(
            app.clone()
                .oneshot(authed("/opds/v1.2/smart-lists", "u3-key"))
                .await
                .unwrap(),
        )
        .await;
        assert!(xml.contains("<title>shared-with-u3</title>"));
        let response = app
            .clone()
            .oneshot(authed(
                &format!("/opds/v1.2/smart-lists/{}", shared_list.id),
                "u3-key",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let xml = body_string(
            app.clone()
                .oneshot(authed("/opds/v1.2/smart-lists", "u4-key"))
                .await
                .unwrap(),
        )
        .await;
        assert!(!xml.contains("shared-with-u3"));
        let response = app
            .oneshot(authed(
                &format!("/opds/v1.2/smart-lists/{}", shared_list.id),
                "u4-key",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let _ = public_list;
    }
}
