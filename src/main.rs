#![warn(clippy::all, clippy::pedantic)]

use ahash::AHashMap;
use parking_lot::RwLock;
use sqlx::{
    query,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::{
    error::Error,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use twilight_gateway::{CloseFrame, Event, Intents, Shard, ShardId};
use twilight_http::Client;
use twilight_model::{
    channel::Message,
    id::{
        marker::{GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};

#[macro_use]
extern crate tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();
    let token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN required in the environment");
    let role_id: Id<RoleMarker> = std::env::var("ROLE_ID")
        .expect("ROLE_ID required in the environment")
        .parse()
        .expect("ROLE_ID must be a valid discord snowflake");
    let guild_id: Id<GuildMarker> = std::env::var("GUILD_ID")
        .expect("GUILD_ID required in the environment")
        .parse()
        .expect("GUILD_ID must be a valid discord snowflake");
    let activity_minutes: i64 = std::env::var("ACTIVITY_MINUTES")
        .expect("ACTIVITY_MINUTES required in the environment")
        .parse()
        .expect("ACTIVITY_MINUTES must be a valid i64");
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL required in the environment");
    let intents = Intents::GUILD_MESSAGES;
    let mut shard = Shard::new(ShardId::ONE, token.clone(), intents);
    let db_opts = SqliteConnectOptions::from_str(&database_url)
        .expect("failed to parse DATABASE_URL")
        .create_if_missing(true)
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let db = SqlitePoolOptions::new()
        .min_connections(5)
        .connect_with(db_opts)
        .await
        .unwrap_or_else(|e| panic!("Failed to connect to {database_url}: {e}"));
    sqlx::migrate!()
        .run(&db)
        .await
        .expect("failed to run migrations");
    info!("created shard");
    let http = Arc::new(Client::new(token));
    let xs = ExpiringSet::with_capacity(1_000);
    let db_sd = db.clone();
    let state = AppState {
        http,
        db,
        xs,
        guild_id,
        role_id,
        activity_minutes,
    };
    let (shutdown_s, mut shutdown_r) = tokio::sync::oneshot::channel();
    debug!("registering shutdown handler");
    #[cfg(not(unix))]
    compile_error!("This application only supports Unix platforms. Consider WSL or docker.");
    tokio::spawn(async move {
        let mut sig =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let ctrlc = tokio::signal::ctrl_c();
        tokio::select! {
            _v = sig.recv() => {},
            _v = ctrlc => {}
        };
        shutdown_s
            .send(())
            .expect("Failed to shut down, is the shutdown handler running?");
        db_sd.close().await;
    });
    loop {
        #[allow(clippy::redundant_pub_crate)]
        let next = tokio::select! {
            v = shard.next_event() => v,
            _ = &mut shutdown_r => break,
        };
        trace!(?next, "got new event");
        let event = match next {
            Ok(event) => event,
            Err(source) => {
                error!(?source, "error receiving event");
                if source.is_fatal() {
                    break;
                }
                continue;
            }
        };
        if let Event::MessageCreate(mc) = event {
            tokio::spawn(handle_message(mc.0, state.clone()));
        }
    }
    let _ = shard.close(CloseFrame::NORMAL).await;
    info!("Shutting down!");
    Ok(())
}

async fn handle_message(mc: Message, state: AppState) {
    if mc.author.bot
        || !mc.guild_id.is_some_and(|v| v == state.guild_id)
        || state.xs.check_add(mc.author.id)
    {
        debug!("skipped {}", mc.author.id);
        return;
    }
    let Some(member) = mc.member else {
        warn!("discord did not send a member object");
        return;
    };
    #[allow(clippy::cast_possible_wrap)]
    let author_id_i64 = mc.author.id.get() as i64;
    let active_minutes = match query!(
        "INSERT INTO users
        (user, active_minutes)
        VALUES (?, 1)
        ON CONFLICT DO UPDATE SET
        active_minutes = active_minutes + 1
        WHERE user = ?
        RETURNING active_minutes",
        author_id_i64,
        author_id_i64
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(v) => v,
        Err(source) => {
            warn!(?source, "failed to add activity to db");
            return;
        }
    }
    .active_minutes;
    if member.roles.contains(&state.role_id) {
        debug!(
            "skipping {} because they already have the role",
            mc.author.id
        );
        return;
    }
    if active_minutes >= state.activity_minutes {
        if let Err(source) = state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await
        {
            warn!(?source, "failed to add role to member");
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub http: Arc<Client>,
    pub db: SqlitePool,
    pub xs: ExpiringSet,
    pub guild_id: Id<GuildMarker>,
    pub role_id: Id<RoleMarker>,
    pub activity_minutes: i64,
}

#[derive(Clone, Debug)]
pub struct ExpiringSet {
    set: Arc<RwLock<ExpiringSetMap>>,
    last_vac: Arc<RwLock<Instant>>,
}

type ExpiringSetMap = AHashMap<Id<UserMarker>, Instant>;

impl ExpiringSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            set: Arc::new(RwLock::new(AHashMap::with_capacity(capacity))),
            last_vac: Arc::new(RwLock::new(Instant::now())),
        }
    }
    /// Checks if the user is in the set. If not, adds them.
    #[must_use]
    pub fn check_add(&self, user: Id<UserMarker>) -> bool {
        trace!("es: {self:?} ct: {:?}", Instant::now());
        if let Some(v) = self.set.write().get(&user) {
            if v.lt(&Instant::now()) {
                self.set.write().remove(&user);
            }
        }
        if self.set.read().contains_key(&user) {
            return true;
        }
        if self.set.read().len() > 1000
            || self
                .last_vac
                .read()
                .gt(&(Instant::now() + Duration::from_secs(20)))
        {
            self.set
                .write()
                .retain(|_id, expiry| *expiry < Instant::now());
            self.set.write().shrink_to_fit();
        }
        self.set
            .write()
            .insert(user, Instant::now() + Duration::from_secs(60));
        trace!("es: {self:?} ct: {:?}", Instant::now());
        false
    }
}

impl Default for ExpiringSet {
    fn default() -> Self {
        Self::with_capacity(16)
    }
}
