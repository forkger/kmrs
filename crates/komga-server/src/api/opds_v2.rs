//! OPDS v2 endpoints (`Opds2Controller.kt`): JSON feeds, the authentication document,
//! WebPub manifests, and the 401 entry-point behavior for `/opds/v2/**`.
//!
//! WebPub DTOs and publication generation come from `crate::webpub` (the shared port of
//! `WebPubGenerator.kt` / `OpdsGenerator.kt`).

use crate::api::restriction;
use crate::auth::MaybeAuth;
use crate::error::ApiError;
use crate::http::base_url::base_url;
use crate::http::headers::{check_not_modified, content_disposition, format_http_date};
use crate::http::pagination::{QueryExt, QueryPageable};
use crate::search_index::searcher;
use crate::state::AppState;
use crate::webpub::{
    decode_epub_extension_view, to_manifest_divina, to_manifest_epub, to_manifest_pdf,
    to_opds_publication_dto, WPLinkDto, WPPublicationDto,
};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Json, Router};
use komga_core::dto::series::SeriesDto;
use komga_core::dto::url_to_file_path;
use komga_core::model::library::Library;
use komga_core::model::media::{Media, MediaStatus};
use komga_core::model::user::{KomgaUser, UserRole};
use komga_core::search::{
    BookSearch, BooleanOp, Equality, ReadStatus, SearchConditionBook, SearchConditionSeries,
    SearchContext, SeriesSearch, StringOp,
};
use komga_db::dao::book::BookDao;
use komga_db::dao::library::LibraryDao;
use komga_db::dao::media::MediaDao;
use komga_db::dao::series::SeriesMetadataDao;
use komga_db::dto_dao::book::BookDtoDao;
use komga_db::dto_dao::collection::CollectionDtoDao;
use komga_db::dto_dao::readlist::ReadListDtoDao;
use komga_db::dto_dao::referential::ReferentialDao;
use komga_db::dto_dao::series::SeriesDtoDao;
use komga_db::dto_dao::{PageRequest, SortOrder};
use komga_media::image::ImageType;
use komga_media::{container, detect};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use time::OffsetDateTime;

// region constants (`interfaces/api/dto/Constants.kt`, `OpdsLinkRel.kt`)

const MEDIATYPE_OPDS_JSON: &str = "application/opds+json";
const MEDIATYPE_OPDS_PUBLICATION_JSON: &str = "application/opds-publication+json";
const MEDIATYPE_OPDS_AUTHENTICATION_JSON: &str = "application/opds-authentication+json";
const MEDIATYPE_DIVINA_JSON: &str = "application/divina+json";
const MEDIATYPE_WEBPUB_JSON: &str = "application/webpub+json";
const MEDIATYPE_PROGRESSION_JSON: &str = "application/vnd.readium.progression+json";
const PROFILE_DIVINA: &str = "https://readium.org/webpub-manifest/profiles/divina";
const PROFILE_EPUB: &str = "https://readium.org/webpub-manifest/profiles/epub";
const PROFILE_PDF: &str = "https://readium.org/webpub-manifest/profiles/pdf";
const REL_PROGRESSION_API: &str = "http://www.cantook.com/api/progression";
const WEBPUB_CONTEXT: &str = "https://readium.org/webpub-manifest/context.jsonld";

mod rel {
    pub const START: &str = "start";
    pub const PREVIOUS: &str = "previous";
    pub const NEXT: &str = "next";
    pub const SELF: &str = "self";
    pub const SEARCH: &str = "search";
    pub const SUBSECTION: &str = "subsection";
    pub const ACQUISITION: &str = "http://opds-spec.org/acquisition";
    pub const AUTH: &str = "http://opds-spec.org/auth/document";
}

const RECOMMENDED_ITEMS_NUMBER: u32 = 5;
const SEGMENTS: &[&str] = &["opds", "v2"];

// endregion

// region DTOs (`WepPub.kt`, `Opds2Dto.kt`, `OpdsAuthDto.kt`)

/// `FeedMetadataDto` is NON_EMPTY; the `page` property itself is `@JsonIgnore` and only
/// surfaces through its derived fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FeedMetadataDto {
    pub title: String,
    #[serde(rename = "subTitle", skip_serializing_if = "Option::is_none")]
    pub sub_title: Option<String>,
    #[serde(rename = "@type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(with = "zoned_date_time_opt", skip_serializing_if = "Option::is_none")]
    pub modified: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "itemsPerPage", skip_serializing_if = "Option::is_none")]
    pub items_per_page: Option<u32>,
    #[serde(rename = "currentPage", skip_serializing_if = "Option::is_none")]
    pub current_page: Option<u32>,
    #[serde(rename = "numberOfItems", skip_serializing_if = "Option::is_none")]
    pub number_of_items: Option<i64>,
}

/// `FeedDto` is NON_EMPTY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FeedDto {
    pub metadata: FeedMetadataDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WPLinkDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub navigation: Vec<WPLinkDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<FacetDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<FeedGroupDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publications: Vec<WPPublicationDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FeedGroupDto {
    pub metadata: FeedMetadataDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WPLinkDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub navigation: Vec<WPLinkDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publications: Vec<WPPublicationDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FacetDto {
    pub metadata: FeedMetadataDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WPLinkDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthenticationDocumentDto {
    pub authentication: Vec<AuthenticationFlowDto>,
    pub title: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WPLinkDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthenticationFlowDto {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<LabelsDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WPLinkDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelsDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Jackson `ISO_ZONED_DATE_TIME`: fraction in 3-digit groups when nanos is non-zero,
/// offset rendered as `Z` for UTC or `±HH:MM` otherwise.
mod zoned_date_time_opt {
    use serde::{Deserializer, Serializer};
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(
        dt: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match dt {
            Some(dt) => serializer.serialize_str(&super::format_zoned(dt)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        let s: Option<String> = serde::Deserialize::deserialize(deserializer)?;
        match s {
            Some(s) => {
                OffsetDateTime::parse(&s, &time::format_description::well_known::Iso8601::DEFAULT)
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
            None => Ok(None),
        }
    }
}

fn format_zoned(dt: &OffsetDateTime) -> String {
    let nanos = dt.nanosecond();
    let fraction = if nanos == 0 {
        String::new()
    } else {
        let digits = format!("{nanos:09}");
        let trimmed = digits.trim_end_matches('0');
        format!(".{trimmed}")
    };
    let offset = dt.offset();
    let offset_str = if offset.is_utc() {
        "Z".to_string()
    } else {
        let sign = if offset.is_negative() { '-' } else { '+' };
        let total = offset.whole_seconds().abs();
        format!("{sign}{:02}:{:02}", total / 3600, (total % 3600) / 60)
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}{offset_str}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
}

// endregion

// region WebPub generation (`WebPubGenerator.kt` / `OpdsGenerator.kt`)

fn url_builder(base: &str, path: &str) -> String {
    format!("{base}/opds/v2/{path}")
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

// endregion

// region time-zone handling (`atZone(systemDefault())` / `toZonedDateTime()`)

/// `LocalDateTime.atZone(systemDefault())`: re-labels the naive wall clock with the system zone
fn at_system_zone(dt: OffsetDateTime) -> OffsetDateTime {
    dt.replace_offset(komga_core::time_codec::system_offset_for_wall_clock(dt))
}

/// `LocalDateTime.toZonedDateTime()`: converts the UTC instant to the system zone
fn utc_to_system_zone(dt: OffsetDateTime) -> OffsetDateTime {
    komga_core::time_codec::to_zoned_date_time(dt)
}

fn now_system() -> OffsetDateTime {
    komga_core::time_codec::to_zoned_date_time(OffsetDateTime::now_utc())
}

// endregion

// region helpers (auth, links, feeds, media access)

fn generate_opds_auth_document(base: &str) -> AuthenticationDocumentDto {
    AuthenticationDocumentDto {
        authentication: vec![AuthenticationFlowDto {
            type_: "http://opds-spec.org/auth/basic".to_string(),
            labels: Some(LabelsDto {
                login: Some("Email".to_string()),
                password: Some("Password".to_string()),
            }),
            links: vec![],
        }],
        title: "Komga".to_string(),
        id: url_builder(base, "auth"),
        description: Some("Enter your email and password to authenticate.".to_string()),
        links: vec![
            WPLinkDto {
                rel: Some("help".to_string()),
                href: Some("https://komga.org".to_string()),
                ..Default::default()
            },
            WPLinkDto {
                rel: Some("logo".to_string()),
                href: Some(format!("{base}/android-chrome-512x512.png")),
                ..Default::default()
            },
        ],
    }
}

/// `OpdsAuthenticationEntryPoint`: 401 with the auth document body and the three headers.
fn unauthorized_auth_document(parts: &axum::http::request::Parts, state: &AppState) -> Response {
    let base = base_url(parts, &state.config);
    let mut response = Json(generate_opds_auth_document(&base)).into_response();
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(MEDIATYPE_OPDS_AUTHENTICATION_JSON),
    );
    headers.insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Realm\""),
    );
    headers.insert(
        header::LINK,
        HeaderValue::from_str(&format!(
            "<{base}/opds/v2/auth>; rel=\"{}\"; type=\"{MEDIATYPE_OPDS_AUTHENTICATION_JSON}\"",
            rel::AUTH
        ))
        .expect("valid link header"),
    );
    response
}

fn require_auth<'a>(
    auth: &'a MaybeAuth,
    parts: &axum::http::request::Parts,
    state: &AppState,
) -> Result<&'a crate::auth::Auth, Box<Response>> {
    auth.0
        .as_ref()
        .ok_or_else(|| Box::new(unauthorized_auth_document(parts, state)))
}

fn wp_link(title: &str, rel: &str, href: String, type_: &str) -> WPLinkDto {
    WPLinkDto {
        title: Some(title.to_string()),
        rel: Some(rel.to_string()),
        href: Some(href),
        type_: Some(type_.to_string()),
        ..Default::default()
    }
}

fn link_start(base: &str) -> WPLinkDto {
    wp_link(
        "Home",
        rel::START,
        url_builder(base, "catalog"),
        MEDIATYPE_OPDS_JSON,
    )
}

fn link_search(base: &str) -> WPLinkDto {
    WPLinkDto {
        title: Some("Search".to_string()),
        rel: Some(rel::SEARCH.to_string()),
        type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
        href: Some(format!("{}{{?query}}", url_builder(base, "search"))),
        templated: Some(true),
        ..Default::default()
    }
}

fn link_self_href(href: String) -> WPLinkDto {
    WPLinkDto {
        rel: Some(rel::SELF.to_string()),
        href: Some(href),
        ..Default::default()
    }
}

fn link_page(uri: &str, page: &PageRequest, total: i64) -> Vec<WPLinkDto> {
    let size = page.size.max(1) as i64;
    let total_pages = (total + size - 1) / size;
    let number = page.page as i64;
    let mut links = vec![];
    if number > 0 {
        links.push(WPLinkDto {
            rel: Some(rel::PREVIOUS.to_string()),
            href: Some(format!("{uri}?page={}", number - 1)),
            ..Default::default()
        });
    }
    if number + 1 < total_pages {
        links.push(WPLinkDto {
            rel: Some(rel::NEXT.to_string()),
            href: Some(format!("{uri}?page={}", number + 1)),
            ..Default::default()
        });
    }
    links
}

fn feed_metadata_page(title: &str, page: &PageRequest, total: i64) -> FeedMetadataDto {
    FeedMetadataDto {
        title: title.to_string(),
        items_per_page: Some(page.size),
        current_page: Some(page.page + 1),
        number_of_items: Some(total),
        ..Default::default()
    }
}

fn check_library_access(
    state: &AppState,
    user: &KomgaUser,
    library_id: Option<&str>,
) -> Result<(Option<Library>, Option<BTreeSet<String>>), ApiError> {
    let library = match library_id {
        Some(id) => {
            let Some(library) = LibraryDao::new(state.db.clone()).find_by_id(id)? else {
                return Err(ApiError::not_found(""));
            };
            if !user.can_access_library(&library.id) {
                return Err(ApiError::forbidden(""));
            }
            Some(library)
        }
        None => None,
    };
    let authorized = user.get_authorized_library_ids(
        library_id
            .map(|id| BTreeSet::from([id.to_string()]))
            .as_ref(),
    );
    Ok((library, authorized))
}

fn get_library_navigation(
    state: &AppState,
    user: &KomgaUser,
    base: &str,
    library_id: Option<&str>,
) -> Result<Vec<WPLinkDto>, ApiError> {
    let uri = url_builder(
        base,
        &format!(
            "libraries{}",
            library_id.map(|id| format!("/{id}")).unwrap_or_default()
        ),
    );
    let probe = PageRequest {
        page: 0,
        size: 1,
        unpaged: false,
        sort: vec![],
    };
    let belongs = library_id.map(|id| BTreeSet::from([id.to_string()]));
    let collections = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(state)))
        .find_all(belongs.as_ref(), None, None, &probe, &user.restrictions)?;
    let readlists = ReadListDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(state)))
        .find_all(belongs.as_ref(), None, None, &probe, &user.restrictions)?;
    let mut nav = vec![
        wp_link(
            "Recommended",
            rel::SUBSECTION,
            uri.clone(),
            MEDIATYPE_OPDS_JSON,
        ),
        wp_link(
            "Browse",
            rel::SUBSECTION,
            format!("{uri}/browse"),
            MEDIATYPE_OPDS_JSON,
        ),
    ];
    if !collections.items.is_empty() {
        nav.push(wp_link(
            "Collections",
            rel::SUBSECTION,
            format!("{uri}/collections"),
            MEDIATYPE_OPDS_JSON,
        ));
    }
    if !readlists.items.is_empty() {
        nav.push(wp_link(
            "Read lists",
            rel::SUBSECTION,
            format!("{uri}/readlists"),
            MEDIATYPE_OPDS_JSON,
        ));
    }
    // smart lists are global, not library-scoped, so they only appear on the root catalog
    if library_id.is_none() && crate::service::smart_list::any_visible(state, user)? {
        nav.push(wp_link(
            "Smart lists",
            rel::SUBSECTION,
            "smart-lists".to_string(),
            MEDIATYPE_OPDS_JSON,
        ));
    }
    Ok(nav)
}

fn libraries_feed_group(
    state: &AppState,
    user: &KomgaUser,
    base: &str,
) -> Result<FeedGroupDto, ApiError> {
    let dao = LibraryDao::new(state.db.clone());
    let libraries = if user.can_access_all_libraries() {
        dao.find_all()?
    } else {
        dao.find_all()?
            .into_iter()
            .filter(|l| user.shared_libraries_ids.contains(&l.id))
            .collect()
    };
    Ok(FeedGroupDto {
        metadata: FeedMetadataDto {
            title: "Libraries".to_string(),
            ..Default::default()
        },
        links: vec![link_self_href(url_builder(base, "libraries"))],
        navigation: libraries.iter().map(|l| library_link(base, l)).collect(),
        ..Default::default()
    })
}

fn library_link(base: &str, library: &Library) -> WPLinkDto {
    WPLinkDto {
        title: Some(library.name.clone()),
        href: Some(url_builder(base, &format!("libraries/{}", library.id))),
        type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
        ..Default::default()
    }
}

fn series_link(base: &str, series: &SeriesDto) -> WPLinkDto {
    WPLinkDto {
        title: Some(series.metadata.title.clone()),
        href: Some(url_builder(base, &format!("series/{}", series.id))),
        type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
        ..Default::default()
    }
}

fn collection_link(
    base: &str,
    collection: &komga_core::dto::collection::CollectionDto,
) -> WPLinkDto {
    WPLinkDto {
        title: Some(collection.name.clone()),
        href: Some(url_builder(base, &format!("collections/{}", collection.id))),
        type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
        ..Default::default()
    }
}

fn readlist_link(base: &str, readlist: &komga_core::dto::readlist::ReadListDto) -> WPLinkDto {
    WPLinkDto {
        title: Some(readlist.name.clone()),
        href: Some(url_builder(base, &format!("readlists/{}", readlist.id))),
        type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
        ..Default::default()
    }
}

fn book_dao(state: &AppState) -> BookDtoDao {
    BookDtoDao::new(state.db.clone()).with_searcher(Some(searcher(state)))
}

fn series_dao(state: &AppState) -> SeriesDtoDao {
    SeriesDtoDao::new(state.db.clone()).with_searcher(Some(searcher(state)))
}

fn require_media(state: &AppState, book_id: &str) -> Result<Media, ApiError> {
    MediaDao::new(state.db.clone())
        .find_by_id(book_id)?
        .ok_or_else(|| ApiError::Internal(format!("no media for book {book_id}")))
}

// same source as api/books.rs (extract_page / image_page_response helpers are private there)
fn last_modified_millis(media: &Media) -> i64 {
    (media.last_modified_date.unix_timestamp_nanos() / 1_000_000) as i64
}

fn set_last_modified(response: &mut Response, media: &Media) {
    response.headers_mut().insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&format_http_date(last_modified_millis(media) / 1000)).unwrap(),
    );
}

fn not_modified_response(media: &Media) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    set_last_modified(&mut response, media);
    response
}

fn parse_convert(convert: Option<&str>) -> Result<Option<ImageType>, ApiError> {
    match convert {
        None => Ok(None),
        Some("") => Ok(None),
        Some(c) if c.eq_ignore_ascii_case("jpeg") => Ok(Some(ImageType::Jpeg)),
        Some(c) if c.eq_ignore_ascii_case("png") => Ok(Some(ImageType::Png)),
        Some(c) => Err(ApiError::bad_request(format!(
            "Invalid conversion format: {c}"
        ))),
    }
}

fn map_media_error(error: komga_media::MediaError) -> ApiError {
    match error {
        komga_media::MediaError::NotReady => ApiError::not_found("Book analysis failed"),
        komga_media::MediaError::PageOutOfBounds(_) => {
            ApiError::bad_request("Page number does not exist")
        }
        komga_media::MediaError::Conversion(message) => ApiError::not_found(message),
        komga_media::MediaError::NoSuchFile(path) => {
            tracing::warn!("File not found: {path}");
            ApiError::not_found("File not found, it may have moved")
        }
        komga_media::MediaError::Unsupported { message, .. } => ApiError::Internal(message),
        komga_media::MediaError::EntryNotFound(message) => ApiError::Internal(message),
        error @ (komga_media::MediaError::Other(_) | komga_media::MediaError::NoThumbnail) => {
            ApiError::Internal(error.to_string())
        }
    }
}

// endregion

// region endpoints

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/opds/v2/auth", get(get_auth_document))
        .route("/opds/v2/catalog", get(get_libraries_recommended_root))
        .route("/opds/v2/libraries", get(get_libraries_recommended_root))
        .route("/opds/v2/libraries/{id}", get(get_libraries_recommended))
        .route(
            "/opds/v2/libraries/keep-reading",
            get(get_keep_reading_root),
        )
        .route(
            "/opds/v2/libraries/{id}/keep-reading",
            get(get_keep_reading),
        )
        .route("/opds/v2/libraries/on-deck", get(get_on_deck_root))
        .route("/opds/v2/libraries/{id}/on-deck", get(get_on_deck))
        .route(
            "/opds/v2/libraries/books/latest",
            get(get_latest_books_root),
        )
        .route(
            "/opds/v2/libraries/{id}/books/latest",
            get(get_latest_books),
        )
        .route(
            "/opds/v2/libraries/series/latest",
            get(get_latest_series_root),
        )
        .route(
            "/opds/v2/libraries/{id}/series/latest",
            get(get_latest_series),
        )
        .route("/opds/v2/libraries/browse", get(get_libraries_browse_root))
        .route("/opds/v2/libraries/{id}/browse", get(get_libraries_browse))
        .route(
            "/opds/v2/libraries/collections",
            get(get_libraries_collections_root),
        )
        .route(
            "/opds/v2/libraries/{id}/collections",
            get(get_libraries_collections),
        )
        .route("/opds/v2/collections/{id}", get(get_one_collection))
        .route(
            "/opds/v2/libraries/readlists",
            get(get_libraries_readlists_root),
        )
        .route(
            "/opds/v2/libraries/{id}/readlists",
            get(get_libraries_readlists),
        )
        .route("/opds/v2/readlists/{id}", get(get_one_readlist))
        .route("/opds/v2/smart-lists", get(get_smart_lists_root))
        .route("/opds/v2/smart-lists/{id}", get(get_one_smart_list))
        .route("/opds/v2/series/{id}", get(get_one_series))
        .route("/opds/v2/search", get(get_search_results))
        .route(
            "/opds/v2/books/{bookId}/pages/{pageNumber}",
            get(get_book_page),
        )
        .route("/opds/v2/books/{bookId}/manifest", get(get_webpub_manifest))
        .route(
            "/opds/v2/books/{bookId}/manifest/epub",
            get(get_webpub_manifest_epub),
        )
        .route(
            "/opds/v2/books/{bookId}/manifest/pdf",
            get(get_webpub_manifest_pdf),
        )
        .route(
            "/opds/v2/books/{bookId}/manifest/divina",
            get(get_webpub_manifest_divina),
        )
}

async fn get_auth_document(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _body) = request.into_parts();
    let base = base_url(&parts, &state.config);
    let mut response = Json(generate_opds_auth_document(&base)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(MEDIATYPE_OPDS_AUTHENTICATION_JSON),
    );
    response
}

async fn get_libraries_recommended_root(
    state: State<AppState>,
    auth: MaybeAuth,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_recommended(state.0, auth, request, None).await
}

async fn get_libraries_recommended(
    state: State<AppState>,
    auth: MaybeAuth,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_recommended(state.0, auth, request, Some(id)).await
}

async fn libraries_recommended(
    state: AppState,
    auth: MaybeAuth,
    request: Request,
    library_id: Option<String>,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let (library, authorized) = check_library_access(&state, user, library_id.as_deref())?;
    let ctx = SearchContext::of_user(user);
    let page_5 = PageRequest {
        page: 0,
        size: RECOMMENDED_ITEMS_NUMBER,
        unpaged: false,
        sort: vec![],
    };

    let library_book_condition = |conditions: &mut Vec<SearchConditionBook>| {
        if let Some(l) = &library {
            conditions.insert(
                0,
                SearchConditionBook::LibraryId {
                    operator: Equality::Is {
                        value: l.id.clone(),
                    },
                },
            );
        }
    };

    let mut conditions = vec![
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
    ];
    library_book_condition(&mut conditions);
    let keep_reading = book_dao(&state).find_all(
        &BookSearch {
            condition: Some(SearchConditionBook::AllOf { conditions }),
            full_text_search: None,
        },
        &ctx,
        &PageRequest {
            sort: vec![SortOrder {
                property: "readProgress.readDate".to_string(),
                descending: true,
            }],
            ..page_5.clone()
        },
    )?;

    let on_deck = book_dao(&state).find_all_on_deck(
        &user.id,
        authorized.as_ref(),
        &user.restrictions,
        state.settings.get().recent_reading_since(),
        &page_5,
    )?;

    let mut conditions = vec![
        SearchConditionBook::MediaStatus {
            operator: Equality::Is {
                value: MediaStatus::Ready,
            },
        },
        SearchConditionBook::Deleted {
            deleted: BooleanOp::IsFalse,
        },
    ];
    library_book_condition(&mut conditions);
    let latest_books = book_dao(&state).find_all(
        &BookSearch {
            condition: Some(SearchConditionBook::AllOf { conditions }),
            full_text_search: None,
        },
        &ctx,
        &PageRequest {
            sort: vec![SortOrder {
                property: "createdDate".to_string(),
                descending: true,
            }],
            ..page_5.clone()
        },
    )?;

    let mut series_conditions = vec![
        SearchConditionSeries::Deleted {
            deleted: BooleanOp::IsFalse,
        },
        SearchConditionSeries::OneShot {
            operator: BooleanOp::IsFalse,
        },
    ];
    if let Some(l) = &library {
        series_conditions.insert(
            0,
            SearchConditionSeries::LibraryId {
                operator: Equality::Is {
                    value: l.id.clone(),
                },
            },
        );
    }
    let latest_series = series_dao(&state).find_all(
        &SeriesSearch {
            condition: Some(SearchConditionSeries::AllOf {
                conditions: series_conditions,
            }),
            full_text_search: None,
        },
        None,
        &ctx,
        &PageRequest {
            sort: vec![SortOrder {
                property: "lastModified".to_string(),
                descending: true,
            }],
            ..page_5.clone()
        },
    )?;

    let uri = url_builder(
        &base,
        &format!(
            "libraries{}",
            library
                .as_ref()
                .map(|l| format!("/{}", l.id))
                .unwrap_or_default()
        ),
    );
    let navigation = get_library_navigation(&state, user, &base, library_id.as_deref())?;

    let mut groups = vec![];
    if library.is_none() {
        groups.push(libraries_feed_group(&state, user, &base)?);
    }
    if !keep_reading.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: feed_metadata_page("Keep Reading", &page_5, keep_reading.total),
            links: vec![wp_link(
                "Keep Reading",
                rel::SELF,
                format!("{uri}/keep-reading"),
                MEDIATYPE_OPDS_JSON,
            )],
            publications: keep_reading
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            ..Default::default()
        });
    }
    if !on_deck.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: feed_metadata_page("On Deck", &page_5, on_deck.total),
            links: vec![wp_link(
                "On Deck",
                rel::SELF,
                format!("{uri}/on-deck"),
                MEDIATYPE_OPDS_JSON,
            )],
            publications: on_deck
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            ..Default::default()
        });
    }
    if !latest_books.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: feed_metadata_page("Latest Books", &page_5, latest_books.total),
            links: vec![wp_link(
                "Latest Books",
                rel::SELF,
                format!("{uri}/books/latest"),
                MEDIATYPE_OPDS_JSON,
            )],
            publications: latest_books
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            ..Default::default()
        });
    }
    if !latest_series.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: feed_metadata_page("Latest Series", &page_5, latest_series.total),
            links: vec![wp_link(
                "Latest Series",
                rel::SELF,
                format!("{uri}/series/latest"),
                MEDIATYPE_OPDS_JSON,
            )],
            navigation: latest_series
                .items
                .iter()
                .map(|s| series_link(&base, s))
                .collect(),
            ..Default::default()
        });
    }

    let feed = FeedDto {
        metadata: FeedMetadataDto {
            title: format!(
                "{} - Recommended",
                library
                    .as_ref()
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "All libraries".to_string())
            ),
            modified: Some(
                library
                    .as_ref()
                    .map(|l| at_system_zone(l.last_modified_date))
                    .unwrap_or_else(now_system),
            ),
            ..Default::default()
        },
        links: vec![link_self_href(uri), link_start(&base), link_search(&base)],
        navigation,
        groups,
        ..Default::default()
    };
    Ok(opds_json(Json(feed).into_response()))
}

fn opds_json(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(MEDIATYPE_OPDS_JSON),
    );
    response
}

async fn list_feed(
    state: AppState,
    auth: MaybeAuth,
    request: Request,
    qp: QueryPageable,
    library_id: Option<String>,
    kind: &str,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let (library, authorized) = check_library_access(&state, user, library_id.as_deref())?;
    let ctx = SearchContext::of_user(user);
    let page_request = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![],
    };
    let (title, modified) = (
        library
            .as_ref()
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "All libraries".to_string()),
        library
            .as_ref()
            .map(|l| at_system_zone(l.last_modified_date))
            .unwrap_or_else(now_system),
    );

    let mut links = vec![];
    let mut navigation: Vec<WPLinkDto> = vec![];
    let mut publications: Vec<WPPublicationDto> = vec![];
    let uri;
    let total;

    match kind {
        "keep-reading" => {
            let page = PageRequest {
                sort: vec![SortOrder {
                    property: "readProgress.readDate".to_string(),
                    descending: true,
                }],
                ..page_request.clone()
            };
            let entries = book_dao(&state).find_all(
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
                &ctx,
                &page,
            )?;
            uri = url_builder(
                &base,
                &format!(
                    "libraries{}/keep-reading",
                    library
                        .as_ref()
                        .map(|l| format!("/{}", l.id))
                        .unwrap_or_default()
                ),
            );
            total = entries.total;
            publications = entries
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect();
            links.extend(link_page(&uri, &page, total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: format!("{title} - Keep Reading"),
                        modified: Some(modified),
                        items_per_page: Some(page.size),
                        current_page: Some(page.page + 1),
                        number_of_items: Some(total),
                        ..Default::default()
                    },
                    links: {
                        let mut l =
                            vec![link_self_href(uri), link_start(&base), link_search(&base)];
                        l.extend(links);
                        l
                    },
                    publications,
                    ..Default::default()
                })
                .into_response(),
            ))
        }
        "on-deck" => {
            let entries = book_dao(&state).find_all_on_deck(
                &user.id,
                authorized.as_ref(),
                &user.restrictions,
                state.settings.get().recent_reading_since(),
                &page_request,
            )?;
            uri = url_builder(
                &base,
                &format!(
                    "libraries{}/on-deck",
                    library
                        .as_ref()
                        .map(|l| format!("/{}", l.id))
                        .unwrap_or_default()
                ),
            );
            total = entries.total;
            publications = entries
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect();
            links.extend(link_page(&uri, &page_request, total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: format!("{title} - On Deck"),
                        modified: Some(modified),
                        items_per_page: Some(page_request.size),
                        current_page: Some(page_request.page + 1),
                        number_of_items: Some(total),
                        ..Default::default()
                    },
                    links: {
                        let mut l =
                            vec![link_self_href(uri), link_start(&base), link_search(&base)];
                        l.extend(links);
                        l
                    },
                    publications,
                    ..Default::default()
                })
                .into_response(),
            ))
        }
        "books-latest" => {
            let page = PageRequest {
                sort: vec![SortOrder {
                    property: "createdDate".to_string(),
                    descending: true,
                }],
                ..page_request.clone()
            };
            let mut conditions = vec![
                SearchConditionBook::MediaStatus {
                    operator: Equality::Is {
                        value: MediaStatus::Ready,
                    },
                },
                SearchConditionBook::Deleted {
                    deleted: BooleanOp::IsFalse,
                },
            ];
            if let Some(l) = &library {
                conditions.insert(
                    0,
                    SearchConditionBook::LibraryId {
                        operator: Equality::Is {
                            value: l.id.clone(),
                        },
                    },
                );
            }
            let entries = book_dao(&state).find_all(
                &BookSearch {
                    condition: Some(SearchConditionBook::AllOf { conditions }),
                    full_text_search: None,
                },
                &ctx,
                &page,
            )?;
            uri = url_builder(
                &base,
                &format!(
                    "libraries{}/books/latest",
                    library
                        .as_ref()
                        .map(|l| format!("/{}", l.id))
                        .unwrap_or_default()
                ),
            );
            total = entries.total;
            publications = entries
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect();
            links.extend(link_page(&uri, &page, total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: format!("{title} - Latest Books"),
                        modified: Some(modified),
                        items_per_page: Some(page.size),
                        current_page: Some(page.page + 1),
                        number_of_items: Some(total),
                        ..Default::default()
                    },
                    links: {
                        let mut l =
                            vec![link_self_href(uri), link_start(&base), link_search(&base)];
                        l.extend(links);
                        l
                    },
                    publications,
                    ..Default::default()
                })
                .into_response(),
            ))
        }
        "series-latest" => {
            let page = PageRequest {
                sort: vec![SortOrder {
                    property: "lastModified".to_string(),
                    descending: true,
                }],
                ..page_request.clone()
            };
            let mut conditions = vec![
                SearchConditionSeries::Deleted {
                    deleted: BooleanOp::IsFalse,
                },
                SearchConditionSeries::OneShot {
                    operator: BooleanOp::IsFalse,
                },
            ];
            if let Some(l) = &library {
                conditions.insert(
                    0,
                    SearchConditionSeries::LibraryId {
                        operator: Equality::Is {
                            value: l.id.clone(),
                        },
                    },
                );
            }
            let entries = series_dao(&state).find_all(
                &SeriesSearch {
                    condition: Some(SearchConditionSeries::AllOf { conditions }),
                    full_text_search: None,
                },
                None,
                &ctx,
                &page,
            )?;
            uri = url_builder(
                &base,
                &format!(
                    "libraries{}/series/latest",
                    library
                        .as_ref()
                        .map(|l| format!("/{}", l.id))
                        .unwrap_or_default()
                ),
            );
            total = entries.total;
            navigation = entries
                .items
                .iter()
                .map(|s| series_link(&base, s))
                .collect();
            links.extend(link_page(&uri, &page, total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: format!("{title} - Latest Series"),
                        modified: Some(modified),
                        items_per_page: Some(page.size),
                        current_page: Some(page.page + 1),
                        number_of_items: Some(total),
                        ..Default::default()
                    },
                    links: {
                        let mut l =
                            vec![link_self_href(uri), link_start(&base), link_search(&base)];
                        l.extend(links);
                        l
                    },
                    navigation,
                    ..Default::default()
                })
                .into_response(),
            ))
        }
        _ => unreachable!("unknown feed kind"),
    }
}

async fn get_keep_reading_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, None, "keep-reading").await
}

async fn get_keep_reading(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, Some(id), "keep-reading").await
}

async fn get_on_deck_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, None, "on-deck").await
}

async fn get_on_deck(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, Some(id), "on-deck").await
}

async fn get_latest_books_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, None, "books-latest").await
}

async fn get_latest_books(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, Some(id), "books-latest").await
}

async fn get_latest_series_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, None, "series-latest").await
}

async fn get_latest_series(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    list_feed(state.0, auth, request, qp, Some(id), "series-latest").await
}

async fn get_libraries_browse_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_browse(state.0, auth, request, qp, None).await
}

async fn get_libraries_browse(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_browse(state.0, auth, request, qp, Some(id)).await
}

async fn libraries_browse(
    state: AppState,
    auth: MaybeAuth,
    request: Request,
    qp: QueryPageable,
    library_id: Option<String>,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let (library, authorized) = check_library_access(&state, user, library_id.as_deref())?;
    let ctx = SearchContext::of_user(user);
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: "metadata.titleSort".to_string(),
            descending: false,
        }],
    };

    let publishers = qp.params.all("publisher");
    let mut conditions = vec![];
    if let Some(l) = &library {
        conditions.push(SearchConditionSeries::LibraryId {
            operator: Equality::Is {
                value: l.id.clone(),
            },
        });
    }
    if !publishers.is_empty() {
        conditions.push(SearchConditionSeries::AllOf {
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

    let entries = series_dao(&state).find_all(
        &SeriesSearch {
            condition: Some(SearchConditionSeries::AllOf { conditions }),
            full_text_search: None,
        },
        None,
        &ctx,
        &page,
    )?;

    let uri = url_builder(
        &base,
        &format!(
            "libraries{}/browse",
            library
                .as_ref()
                .map(|l| format!("/{}", l.id))
                .unwrap_or_default()
        ),
    );
    let navigation = get_library_navigation(&state, user, &base, library_id.as_deref())?;

    let publisher_links: Vec<WPLinkDto> = ReferentialDao::new(state.db.clone())
        .find_all_publishers(authorized.as_ref())?
        .into_iter()
        .map(|p| WPLinkDto {
            title: Some(p.clone()),
            href: Some(format!("{uri}?publisher={p}")),
            type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
            ..Default::default()
        })
        .collect();

    let mut groups = vec![FeedGroupDto {
        metadata: FeedMetadataDto {
            title: "Series".to_string(),
            ..Default::default()
        },
        navigation: entries
            .items
            .iter()
            .map(|s| series_link(&base, s))
            .collect(),
        ..Default::default()
    }];
    if !publisher_links.is_empty() {
        groups.push(FeedGroupDto {
            metadata: FeedMetadataDto {
                title: "Publisher".to_string(),
                ..Default::default()
            },
            navigation: publisher_links,
            ..Default::default()
        });
    }

    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: library
                    .as_ref()
                    .map(|l| l.name.clone())
                    .unwrap_or_else(|| "All libraries".to_string()),
                modified: Some(
                    library
                        .as_ref()
                        .map(|l| at_system_zone(l.last_modified_date))
                        .unwrap_or_else(now_system),
                ),
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            navigation,
            groups,
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_libraries_collections_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_collections(state.0, auth, request, qp, None).await
}

async fn get_libraries_collections(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_collections(state.0, auth, request, qp, Some(id)).await
}

async fn libraries_collections(
    state: AppState,
    auth: MaybeAuth,
    request: Request,
    qp: QueryPageable,
    library_id: Option<String>,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let (library, authorized) = check_library_access(&state, user, library_id.as_deref())?;
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: "name".to_string(),
            descending: false,
        }],
    };
    let entries = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            authorized.as_ref(),
            authorized.as_ref(),
            None,
            &page,
            &user.restrictions,
        )?;

    let uri = url_builder(
        &base,
        &format!(
            "libraries{}/collections",
            library
                .as_ref()
                .map(|l| format!("/{}", l.id))
                .unwrap_or_default()
        ),
    );
    let navigation = get_library_navigation(&state, user, &base, library_id.as_deref())?;
    let groups = vec![FeedGroupDto {
        metadata: FeedMetadataDto {
            title: "Collections".to_string(),
            ..Default::default()
        },
        navigation: entries
            .items
            .iter()
            .map(|c| collection_link(&base, c))
            .collect(),
        ..Default::default()
    }];
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: format!(
                    "{} - Collections",
                    library
                        .as_ref()
                        .map(|l| l.name.clone())
                        .unwrap_or_else(|| "All libraries".to_string())
                ),
                modified: Some(
                    library
                        .as_ref()
                        .map(|l| at_system_zone(l.last_modified_date))
                        .unwrap_or_else(now_system),
                ),
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            navigation,
            groups,
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_one_collection(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let authorized = user.get_authorized_library_ids(None);
    let Some(collection) = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_by_id(&id, authorized.as_ref(), &user.restrictions)?
    else {
        return Err(ApiError::not_found(""));
    };
    let sort = if collection.ordered {
        "collection.number"
    } else {
        "metadata.titleSort"
    };
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: sort.to_string(),
            descending: false,
        }],
    };
    let entries = series_dao(&state).find_all(
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
        &page,
    )?;
    let uri = url_builder(&base, &format!("collections/{id}"));
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: collection.name.clone(),
                modified: Some(at_system_zone(collection.last_modified_date)),
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            navigation: entries
                .items
                .iter()
                .map(|s| series_link(&base, s))
                .collect(),
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_libraries_readlists_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_readlists(state.0, auth, request, qp, None).await
}

async fn get_libraries_readlists(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    libraries_readlists(state.0, auth, request, qp, Some(id)).await
}

async fn libraries_readlists(
    state: AppState,
    auth: MaybeAuth,
    request: Request,
    qp: QueryPageable,
    library_id: Option<String>,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let (library, authorized) = check_library_access(&state, user, library_id.as_deref())?;
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: "name".to_string(),
            descending: false,
        }],
    };
    let entries = ReadListDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            authorized.as_ref(),
            authorized.as_ref(),
            None,
            &page,
            &user.restrictions,
        )?;

    let uri = url_builder(
        &base,
        &format!(
            "libraries{}/readlists",
            library
                .as_ref()
                .map(|l| format!("/{}", l.id))
                .unwrap_or_default()
        ),
    );
    let navigation = get_library_navigation(&state, user, &base, library_id.as_deref())?;
    let groups = vec![FeedGroupDto {
        metadata: FeedMetadataDto {
            title: "Read Lists".to_string(),
            ..Default::default()
        },
        navigation: entries
            .items
            .iter()
            .map(|r| readlist_link(&base, r))
            .collect(),
        ..Default::default()
    }];
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: format!(
                    "{} - Read Lists",
                    library
                        .as_ref()
                        .map(|l| l.name.clone())
                        .unwrap_or_else(|| "All libraries".to_string())
                ),
                modified: Some(
                    library
                        .as_ref()
                        .map(|l| at_system_zone(l.last_modified_date))
                        .unwrap_or_else(now_system),
                ),
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            navigation,
            groups,
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_one_readlist(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let authorized = user.get_authorized_library_ids(None);
    let Some(readlist) = ReadListDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_by_id(&id, authorized.as_ref(), &user.restrictions)?
    else {
        return Err(ApiError::not_found(""));
    };
    let sort = if readlist.ordered {
        "readList.number"
    } else {
        "metadata.releaseDate"
    };
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: sort.to_string(),
            descending: false,
        }],
    };
    let entries = book_dao(&state).find_all(
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
        &page,
    )?;
    let uri = url_builder(&base, &format!("readlists/{id}"));
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: readlist.name.clone(),
                modified: Some(at_system_zone(readlist.last_modified_date)),
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            publications: entries
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_smart_lists_root(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let lists = crate::service::smart_list::find_visible_lists(&state, user)?;

    let size = qp.pageable.size.max(1) as usize;
    let offset = (qp.pageable.page as usize) * size;
    let total = lists.len() as i64;
    let items: Vec<komga_core::model::smart_list::SmartList> =
        lists.into_iter().skip(offset).take(size).collect();

    let uri = url_builder(&base, "smart-lists");
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(
        &uri,
        &PageRequest {
            page: qp.pageable.page,
            size: qp.pageable.size,
            unpaged: false,
            sort: vec![],
        },
        total,
    ));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: "Smart lists".to_string(),
                modified: Some(at_system_zone(komga_core::time_codec::now_utc())),
                items_per_page: Some(qp.pageable.size),
                current_page: Some(qp.pageable.page + 1),
                number_of_items: Some(total),
                ..Default::default()
            },
            links,
            // same group shape as the collections/readlists feeds, so clients render
            // every listing feed the same way
            groups: vec![FeedGroupDto {
                metadata: FeedMetadataDto {
                    title: "Smart lists".to_string(),
                    number_of_items: Some(total),
                    ..Default::default()
                },
                navigation: items
                    .iter()
                    .map(|l| {
                        wp_link(
                            &l.name,
                            rel::SUBSECTION,
                            url_builder(&base, &format!("smart-lists/{}", l.id)),
                            MEDIATYPE_OPDS_JSON,
                        )
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_one_smart_list(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let Some(smart_list) = crate::service::smart_list::find_visible(&state, user, &id)? else {
        return Err(ApiError::not_found(""));
    };

    let ctx = SearchContext::of_user(user);
    let searcher = Some(crate::search_index::searcher(&state));
    // the sort property is target-specific: the series DAO drops unknown keys, which
    // would leave SERIES feeds unordered and pagination unstable
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: crate::service::smart_list::opds_sort_property(smart_list.target).to_string(),
            descending: false,
        }],
    };

    let uri = url_builder(&base, &format!("smart-lists/{id}"));
    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];

    match smart_list.target {
        komga_core::model::smart_list::SmartListTarget::Book => {
            let search = crate::service::smart_list::opds_book_search(&smart_list)?;
            let entries = book_dao(&state)
                .with_searcher(searcher)
                .find_all(&search, &ctx, &page)?;
            links.extend(link_page(&uri, &page, entries.total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: smart_list.name.clone(),
                        modified: Some(at_system_zone(smart_list.last_modified_date)),
                        items_per_page: Some(page.size),
                        current_page: Some(page.page + 1),
                        number_of_items: Some(entries.total),
                        ..Default::default()
                    },
                    links,
                    publications: entries
                        .items
                        .iter()
                        .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                        .collect(),
                    ..Default::default()
                })
                .into_response(),
            ))
        }
        komga_core::model::smart_list::SmartListTarget::Series => {
            let search = crate::service::smart_list::opds_series_search(&smart_list)?;
            let entries = series_dao(&state)
                .with_searcher(searcher)
                .find_all(&search, None, &ctx, &page)?;
            links.extend(link_page(&uri, &page, entries.total));
            Ok(opds_json(
                Json(FeedDto {
                    metadata: FeedMetadataDto {
                        title: smart_list.name.clone(),
                        modified: Some(at_system_zone(smart_list.last_modified_date)),
                        items_per_page: Some(page.size),
                        current_page: Some(page.page + 1),
                        number_of_items: Some(entries.total),
                        ..Default::default()
                    },
                    links,
                    navigation: entries
                        .items
                        .iter()
                        .map(|s| series_link(&base, s))
                        .collect(),
                    ..Default::default()
                })
                .into_response(),
            ))
        }
    }
}

async fn get_one_series(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let Some(series) = series_dao(&state).find_by_id(&id, &user.id)? else {
        return Err(ApiError::not_found(""));
    };
    restriction::check_series_dto(user, &series)?;

    let tag = qp.params.first("tag");
    let mut conditions = vec![
        SearchConditionBook::SeriesId {
            operator: Equality::Is {
                value: series.id.clone(),
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
    ];
    if let Some(tag) = tag {
        conditions.push(SearchConditionBook::Tag {
            tag: komga_core::search::EqualityNullable::Is {
                value: tag.to_string(),
            },
        });
    }
    let page = PageRequest {
        page: qp.pageable.page,
        size: qp.pageable.size,
        unpaged: false,
        sort: vec![SortOrder {
            property: "metadata.numberSort".to_string(),
            descending: false,
        }],
    };
    let entries = book_dao(&state).find_all(
        &BookSearch {
            condition: Some(SearchConditionBook::AllOf { conditions }),
            full_text_search: None,
        },
        &SearchContext::of_user(user),
        &page,
    )?;

    let uri = url_builder(&base, &format!("series/{id}"));
    let tag_links: Vec<WPLinkDto> = ReferentialDao::new(state.db.clone())
        .find_all_book_tags_by_series(&series.id, None)?
        .into_iter()
        .map(|t| {
            let is_current = tag == Some(t.as_str());
            WPLinkDto {
                title: Some(t.clone()),
                href: Some(format!("{uri}?tag={t}")),
                type_: Some(MEDIATYPE_OPDS_JSON.to_string()),
                rel: is_current.then(|| rel::SELF.to_string()),
                ..Default::default()
            }
        })
        .collect();
    let facets = if tag_links.is_empty() {
        vec![]
    } else {
        vec![FacetDto {
            metadata: FeedMetadataDto {
                title: "Tag".to_string(),
                ..Default::default()
            },
            links: tag_links,
        }]
    };

    let description = {
        let s = series.metadata.summary.trim();
        if s.is_empty() {
            non_empty(series.books_metadata.summary.clone())
        } else {
            Some(series.metadata.summary.clone())
        }
    };

    let mut links = vec![
        link_self_href(uri.clone()),
        link_start(&base),
        link_search(&base),
    ];
    links.extend(link_page(&uri, &page, entries.total));

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: series.metadata.title.clone(),
                modified: Some(utc_to_system_zone(series.last_modified)),
                description,
                items_per_page: Some(page.size),
                current_page: Some(page.page + 1),
                number_of_items: Some(entries.total),
                ..Default::default()
            },
            links,
            publications: entries
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            facets,
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_search_results(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let user = &auth.user;
    let base = base_url(&parts, &state.config);
    let ctx = SearchContext::of_user(user);
    let page = PageRequest {
        page: 0,
        size: 20,
        unpaged: false,
        sort: vec![SortOrder {
            property: "relevance".to_string(),
            descending: false,
        }],
    };

    let query = qp.params.first("query");
    let query_terms: Vec<&str> = query
        .map(|q| q.split_whitespace().collect())
        .unwrap_or_default();

    let mut series_conditions = vec![
        SearchConditionSeries::OneShot {
            operator: BooleanOp::IsFalse,
        },
        SearchConditionSeries::Deleted {
            deleted: BooleanOp::IsFalse,
        },
    ];
    for term in &query_terms {
        series_conditions.push(SearchConditionSeries::Title {
            title: StringOp::Contains {
                value: term.to_string(),
            },
        });
    }
    let results_series = series_dao(&state).find_all(
        &SeriesSearch {
            condition: Some(SearchConditionSeries::AllOf {
                conditions: series_conditions,
            }),
            full_text_search: None,
        },
        None,
        &ctx,
        &page,
    )?;

    let mut book_conditions = vec![SearchConditionBook::Deleted {
        deleted: BooleanOp::IsFalse,
    }];
    for term in &query_terms {
        book_conditions.push(SearchConditionBook::Title {
            title: StringOp::Contains {
                value: term.to_string(),
            },
        });
    }
    let results_books = book_dao(&state).find_all(
        &BookSearch {
            condition: Some(SearchConditionBook::AllOf {
                conditions: book_conditions,
            }),
            full_text_search: None,
        },
        &ctx,
        &page,
    )?;

    let authorized = user.get_authorized_library_ids(None);
    let results_collections = CollectionDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            authorized.as_ref(),
            authorized.as_ref(),
            query,
            &page,
            &user.restrictions,
        )?;
    let results_readlists = ReadListDtoDao::new(state.db.clone())
        .with_searcher(Some(crate::search_index::searcher(&state)))
        .find_all(
            authorized.as_ref(),
            authorized.as_ref(),
            query,
            &page,
            &user.restrictions,
        )?;

    let mut groups = vec![];
    if !results_series.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: FeedMetadataDto {
                title: "Series".to_string(),
                ..Default::default()
            },
            navigation: results_series
                .items
                .iter()
                .map(|s| series_link(&base, s))
                .collect(),
            ..Default::default()
        });
    }
    if !results_books.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: FeedMetadataDto {
                title: "Books".to_string(),
                ..Default::default()
            },
            publications: results_books
                .items
                .iter()
                .map(|b| to_opds_publication_dto(b, &base, detect::IMAGE_JPEG))
                .collect(),
            ..Default::default()
        });
    }
    if !results_collections.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: FeedMetadataDto {
                title: "Collections".to_string(),
                ..Default::default()
            },
            navigation: results_collections
                .items
                .iter()
                .map(|c| collection_link(&base, c))
                .collect(),
            ..Default::default()
        });
    }
    if !results_readlists.items.is_empty() {
        groups.push(FeedGroupDto {
            metadata: FeedMetadataDto {
                title: "Read Lists".to_string(),
                ..Default::default()
            },
            navigation: results_readlists
                .items
                .iter()
                .map(|r| readlist_link(&base, r))
                .collect(),
            ..Default::default()
        });
    }

    Ok(opds_json(
        Json(FeedDto {
            metadata: FeedMetadataDto {
                title: "Search results".to_string(),
                modified: Some(now_system()),
                ..Default::default()
            },
            links: vec![link_start(&base), link_search(&base)],
            groups,
            ..Default::default()
        })
        .into_response(),
    ))
}

async fn get_book_page(
    state: State<AppState>,
    auth: MaybeAuth,
    qp: QueryPageable,
    Path((book_id, page_number)): Path<(String, i32)>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    auth.require_role(UserRole::PageStreaming)?;
    let convert = parse_convert(qp.params.first("convert"))?;

    let Some(book) = BookDao::new(state.db.clone()).find_by_id(&book_id)? else {
        return Err(ApiError::not_found(""));
    };
    let media = require_media(&state, &book_id)?;
    if check_not_modified(last_modified_millis(&media), &parts.headers) {
        return Ok(not_modified_response(&media));
    }
    restriction::check_book(&state, &auth.user, &book)?;

    let path = std::path::PathBuf::from(url_to_file_path(&book.url));
    let name = book.name.clone();
    let media_clone = media.clone();
    let number = usize::try_from(page_number).unwrap_or(usize::MAX);
    let page_content = tokio::task::spawn_blocking(move || {
        container::get_book_page(&path, &name, &media_clone, number, convert, None)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(map_media_error)?;

    let extension = detect::media_type_to_extension(&page_content.media_type).unwrap_or("jpeg");
    let mut response = Response::new(axum::body::Body::from(page_content.bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(
            "inline",
            &format!("{}-{page_number}{extension}", book.name),
        ))
        .unwrap(),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&detect::media_type_or_default(Some(
            &page_content.media_type,
        )))
        .unwrap(),
    );
    set_last_modified(&mut response, &media);
    Ok(response)
}

async fn get_webpub_manifest(
    state: State<AppState>,
    auth: MaybeAuth,
    Path(book_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let Some(media) = MediaDao::new(state.db.clone()).find_by_id(&book_id)? else {
        return Err(ApiError::not_found(""));
    };
    match container::media_profile(media.media_type.as_deref()) {
        Some(komga_core::search::MediaProfile::Divina) => {
            manifest_divina(&state, &auth.user, &parts, &book_id).await
        }
        Some(komga_core::search::MediaProfile::Pdf) => {
            manifest_pdf(&state, &auth.user, &parts, &book_id).await
        }
        Some(komga_core::search::MediaProfile::Epub) => {
            manifest_epub(&state, &auth.user, &parts, &book_id).await
        }
        None => Err(ApiError::not_found("Book analysis failed")),
    }
}

async fn get_webpub_manifest_epub(
    state: State<AppState>,
    auth: MaybeAuth,
    Path(book_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let Some(book) = book_dao(&state).find_by_id(&book_id, &auth.user.id)? else {
        return Err(ApiError::not_found(""));
    };
    if book.media.media_profile != "EPUB" {
        return Err(ApiError::bad_request(format!(
            "Book media type '{}' not compatible with requested profile",
            book.media.media_type
        )));
    }
    manifest_epub(&state, &auth.user, &parts, &book_id).await
}

async fn get_webpub_manifest_pdf(
    state: State<AppState>,
    auth: MaybeAuth,
    Path(book_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    let Some(book) = book_dao(&state).find_by_id(&book_id, &auth.user.id)? else {
        return Err(ApiError::not_found(""));
    };
    if book.media.media_profile != "PDF" {
        return Err(ApiError::bad_request(format!(
            "Book media type '{}' not compatible with requested profile",
            book.media.media_type
        )));
    }
    manifest_pdf(&state, &auth.user, &parts, &book_id).await
}

async fn get_webpub_manifest_divina(
    state: State<AppState>,
    auth: MaybeAuth,
    Path(book_id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _body) = request.into_parts();
    let auth = match require_auth(&auth, &parts, &state) {
        Ok(a) => a,
        Err(r) => return Ok(*r),
    };
    manifest_divina(&state, &auth.user, &parts, &book_id).await
}

async fn manifest_divina(
    state: &AppState,
    user: &KomgaUser,
    parts: &axum::http::request::Parts,
    book_id: &str,
) -> Result<Response, ApiError> {
    let Some(book) = book_dao(state).find_by_id(book_id, &user.id)? else {
        return Err(ApiError::not_found(""));
    };
    restriction::check_book_dto(state, user, &book)?;
    let media = require_media(state, book_id)?;
    let series_metadata = SeriesMetadataDao::new(state.db.clone())
        .find_by_id(&book.series_id)?
        .ok_or_else(|| ApiError::Internal(format!("no metadata for series {}", book.series_id)))?;
    let base = base_url(parts, &state.config);
    let dto = to_manifest_divina(
        &book,
        &media,
        &series_metadata,
        &base,
        SEGMENTS,
        detect::IMAGE_JPEG,
    );
    Ok(publication_response(dto))
}

async fn manifest_pdf(
    state: &AppState,
    user: &KomgaUser,
    parts: &axum::http::request::Parts,
    book_id: &str,
) -> Result<Response, ApiError> {
    let Some(book) = book_dao(state).find_by_id(book_id, &user.id)? else {
        return Err(ApiError::not_found(""));
    };
    restriction::check_book_dto(state, user, &book)?;
    let media = require_media(state, book_id)?;
    let series_metadata = SeriesMetadataDao::new(state.db.clone())
        .find_by_id(&book.series_id)?
        .ok_or_else(|| ApiError::Internal(format!("no metadata for series {}", book.series_id)))?;
    let base = base_url(parts, &state.config);
    let dto = to_manifest_pdf(
        &book,
        &media,
        &series_metadata,
        &base,
        SEGMENTS,
        detect::IMAGE_JPEG,
    );
    Ok(publication_response(dto))
}

async fn manifest_epub(
    state: &AppState,
    user: &KomgaUser,
    parts: &axum::http::request::Parts,
    book_id: &str,
) -> Result<Response, ApiError> {
    let Some(book) = book_dao(state).find_by_id(book_id, &user.id)? else {
        return Err(ApiError::not_found(""));
    };
    restriction::check_book_dto(state, user, &book)?;
    let media = require_media(state, book_id)?;
    let series_metadata = SeriesMetadataDao::new(state.db.clone())
        .find_by_id(&book.series_id)?
        .ok_or_else(|| ApiError::Internal(format!("no metadata for series {}", book.series_id)))?;
    let base = base_url(parts, &state.config);
    let extension = decode_epub_extension_view(media.extension_value.as_deref());
    let dto = to_manifest_epub(
        &book,
        &media,
        extension.as_ref(),
        &series_metadata,
        &base,
        SEGMENTS,
        detect::IMAGE_JPEG,
    );
    Ok(publication_response(dto))
}

fn publication_response(dto: WPPublicationDto) -> Response {
    let mut response = Json(dto).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(MEDIATYPE_OPDS_PUBLICATION_JSON),
    );
    response
}

// endregion

// region tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::collections::tests::{
        call, exec, get, insert_user, seed_base, test_state, ADMIN_KEY, USER_KEY,
    };
    use axum::body::Body;
    use axum::http::Request;
    use komga_db::pool::Database;
    use serde_json::Value;
    use std::path::PathBuf;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources")
    }

    fn json(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("response is JSON")
    }

    fn admin_id(db: &Database) -> String {
        db.ro()
            .unwrap()
            .query_row("SELECT ID FROM USER WHERE EMAIL = 'admin@x.y'", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn seed_readlist(db: &Database, id: &str, name: &str, ordered: bool, books: &[&str]) {
        exec(
            db,
            "INSERT INTO READLIST (ID, NAME, SUMMARY, ORDERED, BOOK_COUNT) VALUES (?, ?, '', ?, ?)",
            rusqlite::params![id, name, ordered, books.len() as i32],
        );
        for (i, b) in books.iter().enumerate() {
            exec(
                db,
                "INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) VALUES (?, ?, ?)",
                rusqlite::params![id, b, i as i32],
            );
        }
    }

    fn seed_read_progress(db: &Database, book_id: &str, user_id: &str, page: i32, completed: bool) {
        exec(
            db,
            "INSERT INTO READ_PROGRESS (BOOK_ID, USER_ID, PAGE, COMPLETED, READ_DATE, DEVICE_ID, DEVICE_NAME) \
             VALUES (?, ?, ?, ?, '2021-06-01 00:00:00.0', '', '')",
            rusqlite::params![book_id, user_id, page, completed],
        );
    }

    fn seed_rps(db: &Database, series_id: &str, user_id: &str, read: i32, in_progress: i32) {
        exec(
            db,
            "INSERT INTO READ_PROGRESS_SERIES (SERIES_ID, USER_ID, READ_COUNT, IN_PROGRESS_COUNT, MOST_RECENT_READ_DATE) \
             VALUES (?, ?, ?, ?, '2021-06-01 00:00:00.0')",
            rusqlite::params![series_id, user_id, read, in_progress],
        );
    }

    fn seed_tag(db: &Database, book_id: &str, tag: &str) {
        exec(
            db,
            "INSERT INTO BOOK_METADATA_TAG (BOOK_ID, TAG) VALUES (?, ?)",
            rusqlite::params![book_id, tag],
        );
    }

    /// Analyzes a fixture book and seeds the full entity graph (series metadata, aggregation,
    /// book metadata, media with pages/files/extension).
    fn seed_analyzed_book(
        db: &Database,
        library_id: &str,
        series_id: &str,
        book_id: &str,
        fixture: &str,
    ) -> komga_core::model::media::Media {
        let path = fixtures().join(fixture);
        let analysis = komga_media::analyzer::Analyzer::new(3, 600, 15, None).analyze(&path, false);
        let mut media = analysis.media;
        media.book_id = book_id.to_string();
        if let Some(ext) = &analysis.epub_extension {
            media.extension_class = Some(komga_media::analyzer::EPUB_EXTENSION_CLASS.to_string());
            media.extension_value =
                Some(komga_media::analyzer::encode_epub_extension_gz(ext).unwrap());
        }
        let url = format!("file:{}", path.display());
        exec(
            db,
            "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID) \
             VALUES (?, ?, 'file:/l/s/', '2020-01-01 00:00:00.0', ?)",
            rusqlite::params![series_id, series_id, library_id],
        );
        exec(
            db,
            "INSERT INTO SERIES_METADATA (SERIES_ID, STATUS, TITLE, TITLE_SORT, PUBLISHER, LANGUAGE) \
             VALUES (?, 'ONGOING', ?, ?, 'pub', 'ja')",
            rusqlite::params![series_id, series_id, series_id],
        );
        exec(
            db,
            "INSERT INTO BOOK_METADATA_AGGREGATION (SERIES_ID, SUMMARY, SUMMARY_NUMBER) VALUES (?, '', '')",
            [series_id],
        );
        exec(
            db,
            "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID, FILE_SIZE) \
             VALUES (?, ?, ?, '2020-01-01 00:00:00.0', ?, ?, 1000)",
            rusqlite::params![book_id, book_id, url, series_id, library_id],
        );
        exec(
            db,
            "INSERT INTO BOOK_METADATA (BOOK_ID, TITLE, NUMBER, NUMBER_SORT) VALUES (?, ?, '1', 1.0)",
            rusqlite::params![book_id, book_id],
        );
        komga_db::dao::media::MediaDao::new(db.clone())
            .insert(&media)
            .unwrap();
        media
    }

    fn seed_zip_book(db: &Database, library_id: &str, series_id: &str, book_id: &str) {
        seed_analyzed_book(db, library_id, series_id, book_id, "archives/zip.zip");
    }

    fn offset_suffix() -> String {
        let o = komga_core::time_codec::system_offset_at(komga_core::time_codec::now_utc());
        if o.is_utc() {
            "Z".to_string()
        } else {
            let sign = if o.is_negative() { '-' } else { '+' };
            let total = o.whole_seconds().abs();
            format!("{sign}{:02}:{:02}", total / 3600, (total % 3600) / 60)
        }
    }

    #[tokio::test]
    async fn auth_document_shape() {
        let state = test_state();
        seed_base(&state.db);
        let (status, headers, body) = call(&state, router(), get("/opds/v2/auth", "nope")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/opds-authentication+json"
        );
        let doc = json(&body);
        assert_eq!(doc["title"], "Komga");
        assert_eq!(
            doc["description"],
            "Enter your email and password to authenticate."
        );
        assert_eq!(doc["id"], "http://localhost/opds/v2/auth");
        assert_eq!(
            doc["authentication"][0]["type"],
            "http://opds-spec.org/auth/basic"
        );
        assert_eq!(doc["authentication"][0]["labels"]["login"], "Email");
        assert_eq!(doc["authentication"][0]["labels"]["password"], "Password");
        assert_eq!(doc["links"][0]["rel"], "help");
        assert_eq!(doc["links"][0]["href"], "https://komga.org");
        assert_eq!(doc["links"][1]["rel"], "logo");
        assert_eq!(
            doc["links"][1]["href"],
            "http://localhost/android-chrome-512x512.png"
        );
    }

    #[tokio::test]
    async fn unauthorized_returns_auth_document() {
        let state = test_state();
        seed_base(&state.db);
        let request = Request::builder()
            .uri("/opds/v2/catalog")
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = call(&state, router(), request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/opds-authentication+json"
        );
        assert_eq!(
            headers[header::WWW_AUTHENTICATE].to_str().unwrap(),
            "Basic realm=\"Realm\""
        );
        assert_eq!(
            headers[header::LINK].to_str().unwrap(),
            "<http://localhost/opds/v2/auth>; rel=\"http://opds-spec.org/auth/document\"; type=\"application/opds-authentication+json\""
        );
        let doc = json(&body);
        assert_eq!(doc["title"], "Komga");
        assert_eq!(
            doc["authentication"][0]["type"],
            "http://opds-spec.org/auth/basic"
        );
    }

    #[tokio::test]
    async fn catalog_feed_groups() {
        let state = test_state();
        seed_base(&state.db);
        let admin = admin_id(&state.db);
        // s1: b1 read, b2 unread -> on deck b2; s2: b3 in progress -> keep reading b3
        seed_read_progress(&state.db, "b1", &admin, 10, true);
        seed_rps(&state.db, "s1", &admin, 1, 0);
        seed_read_progress(&state.db, "b3", &admin, 3, false);
        seed_rps(&state.db, "s2", &admin, 0, 1);

        let (status, headers, body) =
            call(&state, router(), get("/opds/v2/catalog", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/opds+json"
        );
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries - Recommended");
        assert!(feed["metadata"]["modified"].is_string());
        assert_eq!(feed["links"][0]["rel"], "self");
        assert_eq!(feed["links"][1]["rel"], "start");
        assert_eq!(feed["links"][2]["rel"], "search");
        assert_eq!(feed["links"][2]["templated"], true);
        assert_eq!(
            feed["links"][2]["href"],
            "http://localhost/opds/v2/search{?query}"
        );
        let nav_titles: Vec<&str> = feed["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["title"].as_str().unwrap())
            .collect();
        assert_eq!(nav_titles, ["Recommended", "Browse", "Collections"]);

        let group_titles: Vec<&str> = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["metadata"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            group_titles,
            [
                "Libraries",
                "Keep Reading",
                "On Deck",
                "Latest Books",
                "Latest Series"
            ]
        );
        let groups = feed["groups"].as_array().unwrap();
        assert_eq!(groups[0]["links"][0]["rel"], "self");
        assert_eq!(
            groups[0]["links"][0]["href"],
            "http://localhost/opds/v2/libraries"
        );
        assert_eq!(groups[0]["navigation"].as_array().unwrap().len(), 2);

        assert_eq!(groups[1]["metadata"]["itemsPerPage"], 5);
        assert_eq!(groups[1]["metadata"]["currentPage"], 1);
        assert_eq!(groups[1]["metadata"]["numberOfItems"], 1);
        assert_eq!(groups[1]["publications"].as_array().unwrap().len(), 1);
        assert_eq!(groups[2]["metadata"]["numberOfItems"], 1);
        assert_eq!(groups[3]["metadata"]["numberOfItems"], 4);
        assert_eq!(groups[4]["navigation"].as_array().unwrap().len(), 3);

        // publications carry images and the progression link
        let publication = &groups[1]["publications"][0];
        assert_eq!(publication["images"][0]["type"], "image/jpeg");
        assert_eq!(
            publication["images"][0]["properties"]["authenticate"]["type"],
            "application/opds-authentication+json"
        );
        let link_rels: Vec<Option<&str>> = publication["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["rel"].as_str())
            .collect();
        assert!(link_rels.contains(&Some("self")));
        assert!(link_rels.contains(&Some("http://opds-spec.org/acquisition")));
        assert!(link_rels.contains(&Some("http://www.cantook.com/api/progression")));
    }

    #[tokio::test]
    async fn library_recommended_scoped() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) =
            call(&state, router(), get("/opds/v2/libraries/l1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "l1 - Recommended");
        let group_titles: Vec<&str> = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["metadata"]["title"].as_str().unwrap())
            .collect();
        assert!(!group_titles.contains(&"Libraries"));
        // latest series is scoped to l1
        let latest = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["metadata"]["title"] == "Latest Series")
            .unwrap();
        assert_eq!(latest["navigation"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn keep_reading_feed() {
        let state = test_state();
        seed_base(&state.db);
        let admin = admin_id(&state.db);
        seed_read_progress(&state.db, "b3", &admin, 3, false);
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/keep-reading", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries - Keep Reading");
        assert_eq!(feed["metadata"]["itemsPerPage"], 20);
        assert_eq!(feed["metadata"]["currentPage"], 1);
        assert_eq!(feed["metadata"]["numberOfItems"], 1);
        assert_eq!(feed["publications"].as_array().unwrap().len(), 1);
        assert!(feed["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "self"));
        assert!(feed["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "start"));
        assert!(feed["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "search"));
        // single page: no previous/next links
        assert!(!feed["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "next"));
    }

    #[tokio::test]
    async fn on_deck_feed() {
        let state = test_state();
        seed_base(&state.db);
        let admin = admin_id(&state.db);
        seed_read_progress(&state.db, "b1", &admin, 10, true);
        seed_rps(&state.db, "s1", &admin, 1, 0);
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/on-deck", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries - On Deck");
        assert_eq!(feed["metadata"]["numberOfItems"], 1);
        assert_eq!(feed["publications"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn on_deck_feed_follows_recent_reading_window() {
        let state = test_state();
        seed_base(&state.db);
        let admin = admin_id(&state.db);
        seed_read_progress(&state.db, "b1", &admin, 10, true);
        // series s1 was last read on 2021-06-01
        seed_rps(&state.db, "s1", &admin, 1, 0);
        let on_deck = || async {
            let (status, _, body) = call(
                &state,
                router(),
                get("/opds/v2/libraries/on-deck", ADMIN_KEY),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            json(&body)["metadata"]["numberOfItems"].clone()
        };

        komga_db::dao::settings::SettingsDao::new(state.db.clone())
            .save_setting_i64("KMRS_RECENT_READING_DAYS", 30)
            .unwrap();
        state.settings.reload();
        assert_eq!(on_deck().await, 0);

        exec(
            &state.db,
            "UPDATE READ_PROGRESS_SERIES SET MOST_RECENT_READ_DATE = ? WHERE SERIES_ID = 's1'",
            rusqlite::params![komga_core::time_codec::format_datetime(
                komga_core::time_codec::now_utc()
            )],
        );
        assert_eq!(on_deck().await, 1);
    }

    #[tokio::test]
    async fn browse_feed_publisher_group() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/browse", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries");
        let series_group = &feed["groups"][0];
        assert_eq!(series_group["metadata"]["title"], "Series");
        let series_titles: Vec<&str> = series_group["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["title"].as_str().unwrap())
            .collect();
        assert_eq!(series_titles, ["Alpha", "beta", "Gamma"]);
        let publisher_group = &feed["groups"][1];
        assert_eq!(publisher_group["metadata"]["title"], "Publisher");
        assert_eq!(publisher_group["navigation"].as_array().unwrap().len(), 3);
        assert_eq!(
            publisher_group["navigation"][0]["href"],
            "http://localhost/opds/v2/libraries/browse?publisher=pub-s1"
        );
        // publisher filter narrows the series list
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/browse?publisher=pub-s1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["groups"][0]["navigation"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn collections_list_and_detail() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/collections", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries - Collections");
        assert_eq!(feed["metadata"]["numberOfItems"], 2);
        let names: Vec<&str> = feed["groups"][0]["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["title"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["another", "Best"]);

        let (status, _, body) =
            call(&state, router(), get("/opds/v2/collections/c1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "Best");
        assert!(feed["metadata"]["modified"].is_string());
        let series_titles: Vec<&str> = feed["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["title"].as_str().unwrap())
            .collect();
        // ordered collection: by collection.number
        assert_eq!(series_titles, ["Alpha", "beta"]);
    }

    #[tokio::test]
    async fn readlists_list_and_detail() {
        let state = test_state();
        seed_base(&state.db);
        seed_readlist(&state.db, "r1", "My List", true, &["b1", "b2"]);
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/libraries/readlists", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "All libraries - Read Lists");
        assert_eq!(feed["groups"][0]["navigation"][0]["title"], "My List");

        let (status, _, body) =
            call(&state, router(), get("/opds/v2/readlists/r1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "My List");
        assert_eq!(feed["metadata"]["numberOfItems"], 2);
        assert_eq!(feed["publications"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn series_detail_facets_and_publications() {
        let state = test_state();
        seed_base(&state.db);
        seed_tag(&state.db, "b1", "seinen");
        exec(
            &state.db,
            "UPDATE BOOK_METADATA_AGGREGATION SET SUMMARY = 'agg summary' WHERE SERIES_ID = 's1'",
            [],
        );
        let (status, _, body) = call(&state, router(), get("/opds/v2/series/s1", ADMIN_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "Alpha");
        // metadata.summary is blank -> aggregated books summary
        assert_eq!(feed["metadata"]["description"], "agg summary");
        assert_eq!(feed["metadata"]["numberOfItems"], 2);
        assert_eq!(feed["facets"][0]["metadata"]["title"], "Tag");
        assert_eq!(feed["facets"][0]["links"][0]["title"], "seinen");
        assert_eq!(
            feed["facets"][0]["links"][0]["href"],
            "http://localhost/opds/v2/series/s1?tag=seinen"
        );
        assert!(feed["facets"][0]["links"][0]["rel"].is_null());

        let publication = &feed["publications"][0];
        assert_eq!(publication["metadata"]["numberOfPages"], 10);
        assert_eq!(
            publication["metadata"]["belongsTo"]["series"][0]["name"],
            "Alpha"
        );
        assert_eq!(
            publication["metadata"]["belongsTo"]["series"][0]["position"],
            1.0
        );
        assert_eq!(publication["images"][0]["type"], "image/jpeg");
        let links = publication["links"].as_array().unwrap();
        let self_link = links.iter().find(|l| l["rel"] == "self").unwrap();
        assert_eq!(self_link["type"], "application/divina+json");
        let acquisition = links
            .iter()
            .find(|l| l["rel"] == "http://opds-spec.org/acquisition")
            .unwrap();
        assert_eq!(acquisition["type"], "application/vnd.comicbook+zip");
        let progression = links
            .iter()
            .find(|l| l["rel"] == "http://www.cantook.com/api/progression")
            .unwrap();
        assert_eq!(
            progression["type"],
            "application/vnd.readium.progression+json"
        );
        assert_eq!(
            progression["properties"]["authenticate"]["href"],
            "http://localhost/opds/v2/auth"
        );
    }

    #[tokio::test]
    async fn search_results_groups() {
        let state = test_state();
        seed_base(&state.db);
        seed_readlist(&state.db, "r1", "My List", true, &["b1"]);
        exec(
            &state.db,
            "UPDATE BOOK_METADATA SET TITLE = 'Alpha book' WHERE BOOK_ID = 'b1'",
            [],
        );
        // lucene search drives collections/readlists
        state
            .search_index
            .add_documents(vec![
                komga_search::EntityDoc {
                    entity: komga_core::task::LuceneEntity::Collection,
                    id: "c2".to_string(),
                    fields: vec![("name".to_string(), "another".to_string())],
                },
                komga_search::EntityDoc {
                    entity: komga_core::task::LuceneEntity::ReadList,
                    id: "r1".to_string(),
                    fields: vec![("name".to_string(), "My List".to_string())],
                },
            ])
            .unwrap();
        state.search_index.commit().unwrap();

        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/search?query=alpha", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "Search results");
        let group_titles: Vec<&str> = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["metadata"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(group_titles, ["Series", "Books"]);
        // no self link on the search feed
        assert!(!feed["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "self"));

        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/search?query=another", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        let group_titles: Vec<&str> = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["metadata"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(group_titles, ["Collections"]);

        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/search?query=list", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        let group_titles: Vec<&str> = feed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["metadata"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(group_titles, ["Read Lists"]);
    }

    #[tokio::test]
    async fn pages_endpoint_serves_page_as_is() {
        let state = test_state();
        seed_base(&state.db);
        seed_zip_book(&state.db, "l1", "s9", "b9");
        let (status, headers, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b9/pages/1", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE].to_str().unwrap(), "image/png");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION].to_str().unwrap(),
            "inline; filename=\"=?UTF-8?Q?b9-1.png?=\"; filename*=UTF-8''b9-1.png"
        );
        assert!(headers.contains_key(header::LAST_MODIFIED));
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn manifest_divina_for_zip() {
        let state = test_state();
        seed_base(&state.db);
        seed_zip_book(&state.db, "l1", "s9", "b9");
        let (status, headers, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b9/manifest", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/opds-publication+json"
        );
        let manifest = json(&body);
        assert_eq!(
            manifest["metadata"]["conformsTo"],
            "https://readium.org/webpub-manifest/profiles/divina"
        );
        let reading_order = manifest["readingOrder"].as_array().unwrap();
        assert_eq!(reading_order.len(), 1);
        assert_eq!(reading_order[0]["type"], "image/png");
        assert_eq!(
            reading_order[0]["href"],
            "http://localhost/opds/v2/books/b9/pages/1?contentNegotiation=false"
        );
        // png is a recommended format: no jpeg alternate
        assert!(reading_order[0].get("alternate").is_none());
        assert_eq!(manifest["resources"][0]["type"], "image/jpeg");
        assert_eq!(
            manifest["resources"][0]["href"],
            "http://localhost/opds/v2/books/b9/thumbnail"
        );
    }

    #[tokio::test]
    async fn manifest_divina_alternate_for_non_recommended_page() {
        let state = test_state();
        seed_base(&state.db);
        // build a zip with a BMP page (not in the recommended list)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bmpbook.cbz");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let img = ::image::RgbImage::from_pixel(12, 8, ::image::Rgb([10, 200, 30]));
            let mut bmp = std::io::Cursor::new(Vec::new());
            ::image::DynamicImage::ImageRgb8(img)
                .write_to(&mut bmp, ::image::ImageFormat::Bmp)
                .unwrap();
            zip.start_file("page1.bmp", zip::write::SimpleFileOptions::default())
                .unwrap();
            use std::io::Write;
            zip.write_all(&bmp.into_inner()).unwrap();
            zip.finish().unwrap();
        }
        let analysis = komga_media::analyzer::Analyzer::new(3, 600, 15, None).analyze(&path, false);
        let mut media = analysis.media;
        media.book_id = "b8".to_string();
        exec(
            &state.db,
            "INSERT INTO SERIES (ID, NAME, URL, FILE_LAST_MODIFIED, LIBRARY_ID) VALUES ('s8', 's8', 'file:/l/', '2020-01-01 00:00:00.0', 'l1')",
            [],
        );
        exec(
            &state.db,
            "INSERT INTO SERIES_METADATA (SERIES_ID, STATUS, TITLE, TITLE_SORT, PUBLISHER) VALUES ('s8', 'ONGOING', 's8', 's8', 'pub')",
            [],
        );
        exec(
            &state.db,
            "INSERT INTO BOOK_METADATA_AGGREGATION (SERIES_ID, SUMMARY, SUMMARY_NUMBER) VALUES ('s8', '', '')",
            [],
        );
        exec(
            &state.db,
            "INSERT INTO BOOK (ID, NAME, URL, FILE_LAST_MODIFIED, SERIES_ID, LIBRARY_ID, FILE_SIZE) VALUES ('b8', 'b8', ?, '2020-01-01 00:00:00.0', 's8', 'l1', 100)",
            [format!("file:{}", path.display())],
        );
        exec(
            &state.db,
            "INSERT INTO BOOK_METADATA (BOOK_ID, TITLE, NUMBER, NUMBER_SORT) VALUES ('b8', 'b8', '1', 1.0)",
            [],
        );
        komga_db::dao::media::MediaDao::new(state.db.clone())
            .insert(&media)
            .unwrap();

        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b8/manifest/divina", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let manifest = json(&body);
        let reading_order = manifest["readingOrder"].as_array().unwrap();
        assert_eq!(reading_order.len(), 1);
        assert_eq!(reading_order[0]["type"], "image/bmp");
        assert_eq!(reading_order[0]["alternate"].as_array().unwrap().len(), 1);
        assert_eq!(reading_order[0]["alternate"][0]["type"], "image/jpeg");
        assert_eq!(
            reading_order[0]["alternate"][0]["href"],
            "http://localhost/opds/v2/books/b8/pages/1?contentNegotiation=false&convert=jpeg"
        );
    }

    #[tokio::test]
    async fn manifest_epub_with_toc_and_rendition() {
        let state = test_state();
        seed_base(&state.db);
        let media = seed_analyzed_book(&state.db, "l1", "s7", "b7", "archives/epub3.epub");
        assert_eq!(media.status, MediaStatus::Ready);
        let extension =
            decode_epub_extension_view(media.extension_value.as_deref()).expect("epub extension");

        let (status, headers, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b7/manifest/epub", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE].to_str().unwrap(),
            "application/opds-publication+json"
        );
        let manifest = json(&body);
        assert_eq!(
            manifest["metadata"]["conformsTo"],
            "https://readium.org/webpub-manifest/profiles/epub"
        );
        assert_eq!(
            manifest["metadata"]["rendition"]["layout"],
            if extension.is_fixed_layout == Some(true) {
                "fixed"
            } else {
                "reflowable"
            }
        );
        let epub_pages = media
            .files
            .iter()
            .filter(|f| f.sub_type == Some(komga_core::model::media::MediaFileSubType::EpubPage))
            .count();
        assert_eq!(
            manifest["readingOrder"].as_array().unwrap().len(),
            epub_pages
        );
        assert_eq!(
            manifest["readingOrder"][0]["href"],
            format!(
                "http://localhost/opds/v2/books/b7/resource/{}",
                media
                    .files
                    .iter()
                    .find(|f| f.sub_type
                        == Some(komga_core::model::media::MediaFileSubType::EpubPage))
                    .unwrap()
                    .file_name
            )
        );
        let epub_assets = media
            .files
            .iter()
            .filter(|f| f.sub_type == Some(komga_core::model::media::MediaFileSubType::EpubAsset))
            .count();
        // thumbnail link + one resource per asset
        assert_eq!(
            manifest["resources"].as_array().unwrap().len(),
            epub_assets + 1
        );
        if !extension.toc.is_empty() {
            assert_eq!(
                manifest["toc"].as_array().unwrap().len(),
                extension.toc.len()
            );
        }
        if !extension.page_list.is_empty() {
            assert_eq!(
                manifest["pageList"].as_array().unwrap().len(),
                extension.page_list.len()
            );
        }
    }

    #[tokio::test]
    async fn manifest_profile_mismatch() {
        let state = test_state();
        seed_base(&state.db);
        seed_zip_book(&state.db, "l1", "s9", "b9");
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b9/manifest/pdf", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = json(&body);
        assert_eq!(
            error["message"],
            "400 BAD_REQUEST \"Book media type 'application/zip' not compatible with requested profile\""
        );
        let (status, _, body) = call(
            &state,
            router(),
            get("/opds/v2/books/b9/manifest/epub", ADMIN_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = json(&body);
        assert_eq!(
            error["message"],
            "400 BAD_REQUEST \"Book media type 'application/zip' not compatible with requested profile\""
        );
    }

    #[tokio::test]
    async fn restricted_user_forbidden_and_not_found() {
        let state = test_state();
        seed_base(&state.db);
        // user@x.y shares only l1
        let (status, _, _) = call(&state, router(), get("/opds/v2/libraries/l2", USER_KEY)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = call(&state, router(), get("/opds/v2/series/s3", USER_KEY)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // c2's only series (s3) is in l2: collection is invisible -> 404
        let (status, _, _) = call(&state, router(), get("/opds/v2/collections/c2", USER_KEY)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn libraries_feed_group_for_limited_user() {
        let state = test_state();
        seed_base(&state.db);
        let (status, _, body) = call(&state, router(), get("/opds/v2/catalog", USER_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        let libraries_group = &feed["groups"][0];
        assert_eq!(libraries_group["metadata"]["title"], "Libraries");
        assert_eq!(libraries_group["navigation"].as_array().unwrap().len(), 1);
        assert_eq!(libraries_group["navigation"][0]["title"], "l1");
    }

    #[test]
    fn zoned_datetime_format() {
        let dt = time::OffsetDateTime::from_unix_timestamp(1577836800).unwrap();
        assert_eq!(format_zoned(&dt), "2020-01-01T00:00:00Z");
        let millis = dt + time::Duration::milliseconds(120);
        assert_eq!(format_zoned(&millis), "2020-01-01T00:00:00.12Z");
        let offset_dt = dt.to_offset(time::UtcOffset::from_hms(8, 0, 0).unwrap());
        assert_eq!(format_zoned(&offset_dt), "2020-01-01T08:00:00+08:00");
        assert!(!offset_suffix().is_empty());
    }

    #[tokio::test]
    async fn smart_lists_navigation_and_publications() {
        use komga_core::model::smart_list::{SmartList, SmartListTarget, SmartListVisibility};

        let state = test_state();
        seed_base(&state.db);
        let user_id: String = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT ID FROM USER WHERE EMAIL = 'user@x.y'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let admin_id: String = state
            .db
            .ro()
            .unwrap()
            .query_row("SELECT ID FROM USER WHERE EMAIL = 'admin@x.y'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let make_list = |owner: &str,
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
                    owner_user_id: owner.into(),
                    target,
                    visibility,
                    search_json: search_json.into(),
                    created_date: komga_core::time_codec::now_utc(),
                    last_modified_date: komga_core::time_codec::now_utc(),
                },
                &[],
            )
            .unwrap()
        };
        let l1_books = r#"{"condition":{"libraryId":{"operator":"is","value":"l1"}}}"#;
        let book_list = make_list(
            &user_id,
            "my-books",
            SmartListVisibility::Private,
            SmartListTarget::Book,
            l1_books,
        );
        let series_list = make_list(
            &user_id,
            "my-series",
            SmartListVisibility::Private,
            SmartListTarget::Series,
            l1_books,
        );
        let admin_private = make_list(
            &admin_id,
            "admin-priv",
            SmartListVisibility::Private,
            SmartListTarget::Book,
            l1_books,
        );
        // aggregated series release dates drive the SERIES ordering; seed them directly
        exec(
            &state.db,
            "UPDATE BOOK_METADATA_AGGREGATION SET RELEASE_DATE = '2020-01-01' WHERE SERIES_ID = 's1'",
            [],
        );
        exec(
            &state.db,
            "UPDATE BOOK_METADATA_AGGREGATION SET RELEASE_DATE = '2019-01-01' WHERE SERIES_ID = 's2'",
            [],
        );

        // root catalog links to the smart lists feed
        let (status, _, body) = call(&state, router(), get("/opds/v2/libraries", USER_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&body).contains("smart-lists"),
            "root catalog should link the smart lists feed"
        );

        // nav feed sits in a titled group, same shape as the collections/readlists feeds
        let (status, _, body) = call(&state, router(), get("/opds/v2/smart-lists", USER_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "Smart lists");
        // entries live in the group, not at the top level
        assert!(feed["navigation"].is_null());
        let group = &feed["groups"][0];
        assert_eq!(group["metadata"]["title"], "Smart lists");
        let titles: Vec<&str> = group["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["my-books", "my-series"]);

        // BOOK list → publications of the matched books (l1: b1, b2, b3), ordered by
        // release date ascending: b3 (2019), b1 (2020), b2 (2021)
        let (status, _, body) = call(
            &state,
            router(),
            get(&format!("/opds/v2/smart-lists/{}", book_list.id), USER_KEY),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        assert_eq!(feed["metadata"]["title"], "my-books");
        let publication_titles: Vec<&str> = feed["publications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["metadata"]["title"].as_str().unwrap())
            .collect();
        assert_eq!(publication_titles, ["b3", "b1", "b2"]);

        // SERIES list → navigation into the matched series (l1: s2 before s1)
        let (status, _, body) = call(
            &state,
            router(),
            get(
                &format!("/opds/v2/smart-lists/{}", series_list.id),
                USER_KEY,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let feed = json(&body);
        let nav_titles: Vec<&str> = feed["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap())
            .collect();
        assert_eq!(nav_titles, ["beta", "Alpha"]);

        // another user's private list is invisible; the nav feed hides it too
        let (status, _, _) = call(
            &state,
            router(),
            get(
                &format!("/opds/v2/smart-lists/{}", admin_private.id),
                USER_KEY,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, body) = call(&state, router(), get("/opds/v2/smart-lists", USER_KEY)).await;
        let feed = json(&body);
        let titles: Vec<&str> = feed["groups"][0]["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["my-books", "my-series"]);

        // SHARED scope: visible to the targeted user only
        insert_user(&state.db, "u3@x.y", &[], &[], Default::default(), "u3-key");
        // a user with no visible lists gets no root-catalog link
        let (_, _, body) = call(&state, router(), get("/opds/v2/libraries", "u3-key")).await;
        assert!(!String::from_utf8_lossy(&body).contains("smart-lists"));
        let shared_list = make_list(
            &user_id,
            "shared-with-user",
            SmartListVisibility::Shared,
            SmartListTarget::Book,
            l1_books,
        );
        crate::service::smart_list::dao(&state)
            .set_shares(&shared_list.id, std::slice::from_ref(&user_id))
            .unwrap();
        // the targeted user sees it and gets the root link
        let (_, _, body) = call(&state, router(), get("/opds/v2/smart-lists", USER_KEY)).await;
        let feed = json(&body);
        let titles: Vec<&str> = feed["groups"][0]["navigation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["my-books", "my-series", "shared-with-user"]);
        let (_, _, body) = call(&state, router(), get("/opds/v2/libraries", USER_KEY)).await;
        assert!(String::from_utf8_lossy(&body).contains("smart-lists"));
        // the non-targeted user sees nothing, in the feed and in the root catalog
        let (_, _, body) = call(&state, router(), get("/opds/v2/smart-lists", "u3-key")).await;
        let feed = json(&body);
        // empty navigation is omitted from the serialized group
        assert!(feed["groups"][0]["navigation"].is_null());
        let (status, _, _) = call(
            &state,
            router(),
            get(
                &format!("/opds/v2/smart-lists/{}", shared_list.id),
                "u3-key",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, body) = call(&state, router(), get("/opds/v2/libraries", "u3-key")).await;
        assert!(!String::from_utf8_lossy(&body).contains("smart-lists"));
    }
}
