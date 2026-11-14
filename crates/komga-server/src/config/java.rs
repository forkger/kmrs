//! First-start migration: carries values from the Java komga configuration into the
//! generated `config.toml`. Sources, looked up in the config dir: `application.yml` or
//! `application.yaml`. Parsing is lenient — keys kmrs does not know are ignored, and a
//! malformed file only loses the migration, not the start.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::file::{
    ConfigDuration, FileBooks, FileConfig, FileCors, FileDatabase, FileFonts, FileKobo,
    FileLibraries, FileOAuth2, FileOAuth2Registration, FileSearch, FileServer,
};

/// Returns the migrated configuration and the file it came from, if any source exists.
pub fn migrate(config_dir: &Path) -> Option<(FileConfig, PathBuf)> {
    for name in ["application.yml", "application.yaml"] {
        let path = config_dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_yaml::from_str::<JavaConfig>(&text) {
            Ok(java) => return Some((java.into_file_config(config_dir), path)),
            Err(e) => {
                tracing::warn!(
                    "{}: {e:#}; starting with default configuration",
                    path.display()
                );
                return None;
            }
        }
    }
    None
}

#[derive(Debug, Default, Deserialize)]
struct JavaConfig {
    server: Option<JavaServer>,
    komga: Option<JavaKomga>,
    spring: Option<JavaSpring>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaServer {
    port: Option<u16>,
    forward_headers_strategy: Option<String>,
    shutdown: Option<String>,
    servlet: Option<JavaServlet>,
    error: Option<serde::de::IgnoredAny>,
    tomcat: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaServlet {
    context_path: Option<String>,
    session: Option<JavaSession>,
}

#[derive(Debug, Default, Deserialize)]
struct JavaSession {
    timeout: Option<ConfigDuration>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaKomga {
    config_dir: Option<PathBuf>,
    page_hashing: Option<u32>,
    epub_divina_letter_count_threshold: Option<usize>,
    oauth2_account_creation: Option<bool>,
    oidc_email_verification: Option<bool>,
    file_hashing: Option<bool>,
    libraries_scan_startup: Option<bool>,
    delete_empty_collections: Option<bool>,
    delete_empty_read_lists: Option<bool>,
    database: Option<JavaDatabase>,
    tasks_db: Option<JavaDatabase>,
    sessions_db: Option<JavaDatabase>,
    jdbc_session_enabled: Option<bool>,
    lucene: Option<JavaLucene>,
    fonts: Option<JavaFonts>,
    cors: Option<JavaCors>,
    kobo: Option<JavaKobo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaDatabase {
    file: Option<PathBuf>,
    batch_chunk_size: Option<u32>,
    pool_size: Option<u32>,
    max_pool_size: Option<u32>,
    journal_mode: Option<String>,
    busy_timeout: Option<ConfigDuration>,
    pragmas: Option<HashMap<String, String>>,
    check_local_filesystem: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaLucene {
    data_directory: Option<PathBuf>,
    commit_delay: Option<ConfigDuration>,
    index_analyzer: Option<serde::de::IgnoredAny>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaFonts {
    data_directory: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaCors {
    allowed_origins: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaKobo {
    sync_item_limit: Option<u32>,
    kepubify_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct JavaSpring {
    security: Option<JavaSpringSecurity>,
}

#[derive(Debug, Default, Deserialize)]
struct JavaSpringSecurity {
    oauth2: Option<JavaSpringOauth2>,
}

#[derive(Debug, Default, Deserialize)]
struct JavaSpringOauth2 {
    client: Option<JavaSpringOauth2Client>,
}

#[derive(Debug, Default, Deserialize)]
struct JavaSpringOauth2Client {
    registration: Option<HashMap<String, JavaOauth2Registration>>,
    provider: Option<HashMap<String, JavaOauth2Provider>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaOauth2Registration {
    client_name: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    authorization_grant_type: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct JavaOauth2Provider {
    issuer_uri: Option<String>,
    authorization_uri: Option<String>,
    token_uri: Option<String>,
    user_info_uri: Option<String>,
    user_name_attribute: Option<String>,
}

impl JavaConfig {
    fn into_file_config(self, config_dir: &Path) -> FileConfig {
        let mut file = FileConfig::default();

        if let Some(server) = self.server {
            if server.error.is_some() {
                tracing::warn!("server.error.*: not supported by kmrs, ignored during migration");
            }
            if server.tomcat.is_some() {
                tracing::warn!("server.tomcat.*: tomcat-specific, ignored during migration");
            }
            if let Some(v) = &server.forward_headers_strategy {
                if !v.eq_ignore_ascii_case("framework") {
                    tracing::warn!(
                        "server.forward-headers-strategy={v:?}: kmrs always reads the forwarded headers, X-Forwarded-For only from server.trusted-proxies"
                    );
                }
            }
            if let Some(v) = &server.shutdown {
                if !v.eq_ignore_ascii_case("graceful") {
                    tracing::warn!("server.shutdown={v:?}: kmrs always shuts down gracefully");
                }
            }
            let servlet = server.servlet.unwrap_or_default();
            file.server = Some(FileServer {
                port: server.port,
                context_path: servlet.context_path,
                session_timeout: servlet.session.and_then(|s| s.timeout),
                jdbc_session_enabled: None,
                sort_locale: None,
                trusted_proxies: None,
                body_idle_timeout: None,
                header_read_timeout: None,
            });
        }

        let mut account_creation = None;
        let mut oidc_email_verification = None;
        if let Some(komga) = self.komga {
            // ${komga.config-dir} placeholders in the Java paths refer to the Java data
            // directory, which may differ from the one kmrs is pointed at
            let subst_base = komga
                .config_dir
                .clone()
                .unwrap_or_else(|| config_dir.to_path_buf());
            let subst_base = subst_user_home(&subst_base.to_string_lossy());
            if let Some(d) = &komga.config_dir {
                if subst_base != config_dir {
                    tracing::warn!(
                        "komga.config-dir={} differs from the kmrs config dir {}; migrated paths point at the former",
                        d.display(),
                        config_dir.display()
                    );
                }
            }

            account_creation = komga.oauth2_account_creation;
            oidc_email_verification = komga.oidc_email_verification;
            file.books = Some(FileBooks {
                page_hashing: komga.page_hashing,
                epub_divina_letter_count_threshold: komga.epub_divina_letter_count_threshold,
            });
            file.libraries = Some(FileLibraries {
                // no Komga equivalent
                allow_file_deletion: None,
                file_hashing: komga.file_hashing,
                scan_on_startup: komga.libraries_scan_startup,
                delete_empty_collections: komga.delete_empty_collections,
                delete_empty_read_lists: komga.delete_empty_read_lists,
            });
            file.database = komga
                .database
                .map(|d| d.into_file(&subst_base, "komga.database"));
            file.tasks_db = komga
                .tasks_db
                .map(|d| d.into_file(&subst_base, "komga.tasks-db"));
            file.sessions_db = komga
                .sessions_db
                .map(|d| d.into_file(&subst_base, "komga.sessions-db"));
            if let Some(enabled) = komga.jdbc_session_enabled {
                let server = file.server.get_or_insert_with(FileServer::default);
                server.jdbc_session_enabled = Some(enabled);
            }
            if let Some(lucene) = komga.lucene {
                if lucene.commit_delay.is_some() {
                    tracing::warn!("komga.lucene.commit-delay: not supported by kmrs, ignored during migration");
                }
                if lucene.index_analyzer.is_some() {
                    tracing::warn!("komga.lucene.index-analyzer.*: not supported by kmrs, ignored during migration");
                }
                file.search = Some(FileSearch {
                    data_directory: lucene.data_directory.map(|p| subst_path(p, &subst_base)),
                });
            }
            if let Some(fonts) = komga.fonts {
                file.fonts = Some(FileFonts {
                    data_directory: fonts.data_directory.map(|p| subst_path(p, &subst_base)),
                });
            }
            if let Some(cors) = komga.cors {
                file.cors = Some(FileCors {
                    allowed_origins: cors.allowed_origins,
                });
            }
            if let Some(kobo) = komga.kobo {
                file.kobo = Some(FileKobo {
                    sync_item_limit: kobo.sync_item_limit,
                    kepubify_path: kobo.kepubify_path.map(|p| subst_path(p, &subst_base)),
                });
            }
        }

        let mut registrations: HashMap<String, FileOAuth2Registration> = HashMap::new();
        if let Some(client) = self
            .spring
            .and_then(|s| s.security)
            .and_then(|s| s.oauth2)
            .and_then(|o| o.client)
        {
            // providers fill gaps first; registration values win, like Spring's binding
            for (id, provider) in client.provider.unwrap_or_default() {
                let entry = registrations.entry(id).or_default();
                if entry.issuer_uri.is_none() {
                    entry.issuer_uri = provider.issuer_uri;
                }
                if entry.authorization_uri.is_none() {
                    entry.authorization_uri = provider.authorization_uri;
                }
                if entry.token_uri.is_none() {
                    entry.token_uri = provider.token_uri;
                }
                if entry.user_info_uri.is_none() {
                    entry.user_info_uri = provider.user_info_uri;
                }
                if entry.user_name_attribute.is_none() {
                    entry.user_name_attribute = provider.user_name_attribute;
                }
            }
            for (id, reg) in client.registration.unwrap_or_default() {
                let entry = registrations.entry(id).or_default();
                if let Some(v) = reg.client_name {
                    entry.client_name = Some(v);
                }
                if let Some(v) = reg.client_id {
                    entry.client_id = Some(v);
                }
                if let Some(v) = reg.client_secret {
                    entry.client_secret = Some(v);
                }
                if let Some(v) = reg.authorization_grant_type {
                    entry.authorization_grant_type = Some(v);
                }
                if let Some(v) = reg.redirect_uri {
                    entry.redirect_uri = Some(v);
                }
                if let Some(v) = reg.scope {
                    entry.scopes = Some(v);
                }
            }
        }
        if account_creation.is_some()
            || oidc_email_verification.is_some()
            || !registrations.is_empty()
        {
            file.oauth2 = Some(FileOAuth2 {
                account_creation,
                oidc_email_verification,
                registrations: (!registrations.is_empty()).then_some(registrations),
            });
        }

        file
    }
}

impl JavaDatabase {
    fn into_file(self, subst_base: &Path, toml_path: &str) -> FileDatabase {
        if self.batch_chunk_size.is_some() {
            tracing::warn!(
                "{toml_path}.batch-chunk-size: not supported by kmrs, ignored during migration"
            );
        }
        if self.check_local_filesystem.is_some() {
            tracing::warn!("{toml_path}.check-local-filesystem: not supported by kmrs, ignored during migration");
        }
        // an unparsable mode would fail the resolve later; drop it instead of the migration
        let journal_mode = self.journal_mode.and_then(|mode| {
            super::parse_journal_mode(&mode)
                .map_err(|e| tracing::warn!("{toml_path}.journal-mode: {e:#}, using the default"))
                .ok()
                .map(|_| mode)
        });
        FileDatabase {
            file: self.file.map(|p| subst_path(p, subst_base)),
            pool_size: self.pool_size,
            max_pool_size: self.max_pool_size,
            journal_mode,
            busy_timeout: self.busy_timeout,
            pragmas: self.pragmas,
        }
    }
}

fn subst_path(p: PathBuf, subst_base: &Path) -> PathBuf {
    let s = p
        .to_string_lossy()
        .replace("${komga.config-dir}", &subst_base.to_string_lossy());
    subst_user_home(&s)
}

fn subst_user_home(s: &str) -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(s.replace("${user.home}", &home)),
        Err(_) => PathBuf::from(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrate_yaml(yaml: &str, config_dir: &Path) -> FileConfig {
        let java: JavaConfig = serde_yaml::from_str(yaml).unwrap();
        java.into_file_config(config_dir)
    }

    #[test]
    fn full_application_yml() {
        let file = migrate_yaml(
            r#"
server:
  port: 8080
  forward-headers-strategy: framework
  servlet:
    context-path: /komga
    session:
      timeout: 12h
komga:
  config-dir: /data/komga
  page-hashing: 5
  epub-divina-letter-count-threshold: 20
  oauth2-account-creation: true
  oidc-email-verification: false
  file-hashing: false
  libraries-scan-startup: true
  delete-empty-collections: false
  delete-empty-read-lists: false
  database:
    file: ${komga.config-dir}/database.sqlite
    pool-size: 4
    max-pool-size: 8
    journal-mode: delete
    busy-timeout: 30s
    pragmas:
      synchronous: NORMAL
  tasks-db:
    file: /other/tasks.sqlite
  sessions-db:
    file: /other/sessions.sqlite
    pragmas:
      synchronous: NORMAL
  jdbc-session-enabled: true
  lucene:
    data-directory: ${komga.config-dir}/lucene
  fonts:
    data-directory: ${komga.config-dir}/fonts
  cors:
    allowed-origins:
      - https://a.example
  kobo:
    sync-item-limit: 50
    kepubify-path: /usr/local/bin/kepubify
spring:
  security:
    oauth2:
      client:
        registration:
          github:
            client-id: gh-id
            client-secret: gh-secret
            client-name: GitHub
            scope:
              - read:user
        provider:
          github:
            issuer-uri: https://github.com
            token-uri: https://github.com/token
logging:
  level:
    root: DEBUG
"#,
            Path::new("/data/komga"),
        );
        let server = file.server.unwrap();
        assert_eq!(server.port, Some(8080));
        assert_eq!(server.context_path.as_deref(), Some("/komga"));
        assert_eq!(
            server.session_timeout.unwrap().0,
            std::time::Duration::from_secs(12 * 3600)
        );
        let books = file.books.unwrap();
        assert_eq!(books.page_hashing, Some(5));
        assert_eq!(books.epub_divina_letter_count_threshold, Some(20));
        let libraries = file.libraries.unwrap();
        assert_eq!(libraries.file_hashing, Some(false));
        assert_eq!(libraries.scan_on_startup, Some(true));
        assert_eq!(libraries.delete_empty_collections, Some(false));
        assert_eq!(libraries.delete_empty_read_lists, Some(false));
        let database = file.database.unwrap();
        assert_eq!(
            database.file,
            Some(PathBuf::from("/data/komga/database.sqlite"))
        );
        assert_eq!(database.pool_size, Some(4));
        assert_eq!(database.max_pool_size, Some(8));
        assert_eq!(database.journal_mode.as_deref(), Some("delete"));
        assert_eq!(
            database.busy_timeout.unwrap().0,
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            database.pragmas.unwrap().get("synchronous"),
            Some(&"NORMAL".to_string())
        );
        assert_eq!(
            file.tasks_db.unwrap().file,
            Some(PathBuf::from("/other/tasks.sqlite"))
        );
        let sessions = file.sessions_db.unwrap();
        assert_eq!(sessions.file, Some(PathBuf::from("/other/sessions.sqlite")));
        assert_eq!(
            sessions.pragmas.unwrap().get("synchronous"),
            Some(&"NORMAL".to_string())
        );
        assert_eq!(server.jdbc_session_enabled, Some(true));
        assert_eq!(
            file.search.unwrap().data_directory,
            Some(PathBuf::from("/data/komga/lucene"))
        );
        assert_eq!(
            file.fonts.unwrap().data_directory,
            Some(PathBuf::from("/data/komga/fonts"))
        );
        assert_eq!(
            file.cors.unwrap().allowed_origins,
            Some(vec!["https://a.example".to_string()])
        );
        let kobo = file.kobo.unwrap();
        assert_eq!(kobo.sync_item_limit, Some(50));
        assert_eq!(
            kobo.kepubify_path,
            Some(PathBuf::from("/usr/local/bin/kepubify"))
        );
        let oauth2 = file.oauth2.unwrap();
        assert_eq!(oauth2.account_creation, Some(true));
        assert_eq!(oauth2.oidc_email_verification, Some(false));
        let registrations = oauth2.registrations.unwrap();
        let github = &registrations["github"];
        assert_eq!(github.client_id.as_deref(), Some("gh-id"));
        assert_eq!(github.client_secret.as_deref(), Some("gh-secret"));
        assert_eq!(github.client_name.as_deref(), Some("GitHub"));
        assert_eq!(github.scopes, Some(vec!["read:user".to_string()]));
        assert_eq!(github.issuer_uri.as_deref(), Some("https://github.com"));
        assert_eq!(
            github.token_uri.as_deref(),
            Some("https://github.com/token")
        );
    }

    #[test]
    fn provider_fields_merge_into_registration() {
        let file = migrate_yaml(
            r#"
spring:
  security:
    oauth2:
      client:
        registration:
          okta:
            client-id: okta-id
        provider:
          okta:
            issuer-uri: https://okta.example
            token-uri: https://okta.example/token
"#,
            Path::new("/data"),
        );
        let registrations = file.oauth2.unwrap().registrations.unwrap();
        let okta = &registrations["okta"];
        assert_eq!(okta.client_id.as_deref(), Some("okta-id"));
        assert_eq!(okta.issuer_uri.as_deref(), Some("https://okta.example"));
        assert_eq!(
            okta.token_uri.as_deref(),
            Some("https://okta.example/token")
        );
    }

    #[test]
    fn config_dir_placeholder_uses_java_dir() {
        let file = migrate_yaml(
            r#"
komga:
  config-dir: /java/data
  database:
    file: ${komga.config-dir}/database.sqlite
"#,
            Path::new("/rust/data"),
        );
        assert_eq!(
            file.database.unwrap().file,
            Some(PathBuf::from("/java/data/database.sqlite"))
        );
    }

    #[test]
    fn invalid_journal_mode_is_dropped() {
        let file = migrate_yaml(
            "komga:\n  database:\n    journal-mode: bogus\n",
            Path::new("/data"),
        );
        assert_eq!(file.database.unwrap().journal_mode, None);
    }

    #[test]
    fn jdbc_session_opt_out_is_carried_over() {
        let file = migrate_yaml(
            "komga:\n  jdbc-session-enabled: false\n",
            Path::new("/data"),
        );
        assert_eq!(file.server.unwrap().jdbc_session_enabled, Some(false));
    }

    #[test]
    fn migrate_picks_up_sources() {
        let dir = tempfile::tempdir().unwrap();
        assert!(migrate(dir.path()).is_none());

        std::fs::write(
            dir.path().join("application.yaml"),
            "server:\n  port: 8081\n",
        )
        .unwrap();
        let (file, source) = migrate(dir.path()).unwrap();
        assert!(source.ends_with("application.yaml"));
        assert_eq!(file.server.unwrap().port, Some(8081));
        std::fs::remove_file(dir.path().join("application.yaml")).unwrap();

        std::fs::write(
            dir.path().join("application.yml"),
            "komga:\n  page-hashing: 9\n",
        )
        .unwrap();
        let (file, source) = migrate(dir.path()).unwrap();
        assert!(source.ends_with("application.yml"));
        assert_eq!(file.books.unwrap().page_hashing, Some(9));
    }
}
