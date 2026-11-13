use super::login_throttle::LoginThrottle;
use super::password_cache::{PasswordCache, VerifiedPassword};
use super::{ActivityDraft, Auth, AuthSource, Outcome, SessionStore, SOURCE_PASSWORD};
use crate::error::ApiError;
use komga_core::model::user::KomgaUser;
use komga_db::dao::user::UserDao;
use komga_db::pool::Database;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const PASSWORD_CHECK_WAIT: Duration = Duration::from_secs(2);

pub(super) struct PasswordChecksBusy;

enum PasswordAttempt {
    Complete(Outcome),
    Check(KomgaUser, VerifiedPassword),
}

pub(crate) struct UserAuthentication {
    users: UserDao,
    sessions: SessionStore,
    checks: Arc<Semaphore>,
    throttle: LoginThrottle,
    cache: PasswordCache,
    updates: RwLock<()>,
}

impl UserAuthentication {
    pub fn new(db: Database, sessions: SessionStore) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |count| count.get());
        Self {
            users: UserDao::new(db),
            sessions,
            checks: Arc::new(Semaphore::new((cores / 2).max(1))),
            throttle: LoginThrottle::new(),
            cache: PasswordCache::new(),
            updates: RwLock::new(()),
        }
    }

    pub(super) async fn authenticate(
        &self,
        email: String,
        password: &str,
        client: Option<IpAddr>,
    ) -> Result<Outcome, PasswordChecksBusy> {
        if let PasswordAttempt::Complete(outcome) = self.prepare(&email, password, client) {
            return Ok(outcome);
        }
        let Ok(Ok(_permit)) =
            tokio::time::timeout(PASSWORD_CHECK_WAIT, self.checks.clone().acquire_owned()).await
        else {
            report_busy_password_checks();
            return Err(PasswordChecksBusy);
        };
        let (user, key) = match self.prepare(&email, password, client) {
            PasswordAttempt::Complete(outcome) => return Ok(outcome),
            PasswordAttempt::Check(user, key) => (user, key),
        };
        if bcrypt::verify(password, &user.password).unwrap_or(false) {
            return Ok(self.finish(user, key, client));
        }
        if let Some(lock) = self
            .throttle
            .record_failure(&user.id, client, Instant::now())
        {
            tracing::warn!(
                "password logins for {email:?} from {} refused for {}s after repeated failures",
                client.map_or_else(|| "an unknown address".to_string(), |ip| ip.to_string()),
                lock.as_secs()
            );
        }
        Ok(failed_password_outcome(None, email, "Bad credentials"))
    }

    fn prepare(&self, email: &str, password: &str, client: Option<IpAddr>) -> PasswordAttempt {
        let _guard = self.updates.read().unwrap();
        let Some(user) = self.users.find_by_email_ignore_case(email).ok().flatten() else {
            return PasswordAttempt::Complete(failed_password_outcome(
                None,
                email.to_string(),
                "Bad credentials",
            ));
        };
        if self.throttle.is_locked(&user.id, client, Instant::now()) {
            return PasswordAttempt::Complete(failed_password_outcome(
                Some(user.id),
                email.to_string(),
                "Too many failed attempts",
            ));
        }
        let key = self.cache.key(&user.id, &user.password, client, password);
        if self.cache.contains(&key) {
            PasswordAttempt::Complete(self.verified_outcome(user, client))
        } else {
            PasswordAttempt::Check(user, key)
        }
    }

    fn finish(
        &self,
        checked_user: KomgaUser,
        key: VerifiedPassword,
        client: Option<IpAddr>,
    ) -> Outcome {
        let _guard = self.updates.read().unwrap();
        let Some(user) = self
            .users
            .find_by_id(&checked_user.id)
            .ok()
            .flatten()
            .filter(|user| user.password == checked_user.password)
        else {
            return failed_password_outcome(None, checked_user.email, "Bad credentials");
        };
        self.cache.insert(key);
        self.verified_outcome(user, client)
    }

    fn verified_outcome(&self, user: KomgaUser, client: Option<IpAddr>) -> Outcome {
        self.throttle.record_success(&user.id, client);
        let activity = ActivityDraft {
            user_id: Some(user.id.clone()),
            email: Some(user.email.clone()),
            api_key_id: None,
            api_key_comment: None,
            success: true,
            error: None,
            source: SOURCE_PASSWORD.into(),
        };
        Outcome {
            new_session_id: self
                .sessions
                .create(&user.id)
                .map_err(|error| {
                    tracing::warn!("failed to persist session, continuing without one: {error}")
                })
                .ok(),
            auth: Some(Auth {
                user,
                source: AuthSource::Password,
                api_key_id: None,
                session_id: None,
            }),
            activity: Some(activity),
        }
    }

    pub fn update_user(
        &self,
        id: &str,
        actor_id: &str,
        update: impl FnOnce(&mut KomgaUser) -> Result<(), ApiError>,
    ) -> Result<(KomgaUser, bool), ApiError> {
        let _guard = self.updates.write().unwrap();
        let mut user = self
            .users
            .find_by_id(id)?
            .ok_or_else(|| ApiError::not_found(""))?;
        let before = user.clone();
        update(&mut user)?;
        let expire_sessions = before.roles != user.roles
            || before.restrictions != user.restrictions
            || before.shared_all_libraries != user.shared_all_libraries
            || before.shared_libraries_ids != user.shared_libraries_ids
            || (before.password != user.password && actor_id != id);
        self.users.update(&user)?;
        if expire_sessions {
            self.sessions.invalidate_user(id)?;
        }
        Ok((user, expire_sessions))
    }

    pub fn delete_user(&self, id: &str) -> Result<KomgaUser, ApiError> {
        let _guard = self.updates.write().unwrap();
        let user = self
            .users
            .find_by_id(id)?
            .ok_or_else(|| ApiError::not_found(""))?;
        self.users.delete(id, &user.email)?;
        self.sessions.invalidate_user(id)?;
        Ok(user)
    }
}

fn failed_password_outcome(user_id: Option<String>, email: String, error: &str) -> Outcome {
    Outcome {
        auth: None,
        new_session_id: None,
        activity: Some(ActivityDraft {
            user_id,
            email: Some(email),
            api_key_id: None,
            api_key_comment: None,
            success: false,
            error: Some(error.into()),
            source: SOURCE_PASSWORD.into(),
        }),
    }
}

fn report_busy_password_checks() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    const INTERVAL: Duration = Duration::from_secs(60);
    static TOLD_TO_RETRY: AtomicU64 = AtomicU64::new(0);
    static REPORTING: AtomicBool = AtomicBool::new(false);

    TOLD_TO_RETRY.fetch_add(1, Ordering::Relaxed);
    if !REPORTING.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            "password checks are saturated: logins that cannot start within {}s are answered 503",
            PASSWORD_CHECK_WAIT.as_secs()
        );
        tokio::spawn(async {
            tokio::time::sleep(INTERVAL).await;
            let told_to_retry = TOLD_TO_RETRY.swap(0, Ordering::Relaxed);
            REPORTING.store(false, Ordering::Release);
            tracing::warn!(
                "{told_to_retry} login request(s) were answered 503 in the last {}s",
                INTERVAL.as_secs()
            );
        });
    }
}

#[cfg(test)]
mod tests;
