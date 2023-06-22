#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

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
    let reward_level: u64 = std::env::var("REWARD_LEVEL")
        .expect("REWARD_LEVEL required in the environment")
        .parse()
        .expect("REWARD_LEVEL must be a valid i32");
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
    tracing::info!("created shard");
    let http = Arc::new(Client::new(token));
    let xs = ExpiringSet::with_capacity(1_000);
    let state = AppState {
        http,
        db,
        xs,
        guild_id,
        role_id,
        reward_level,
    };
    let (shutdown_s, mut shutdown_r) = tokio::sync::oneshot::channel();
    tokio::spawn(async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to wait for ctrl+c!");
        shutdown_s
            .send(())
            .expect("Failed to shut down, is the shutdown handler running?");
    });
    loop {
        #[allow(clippy::redundant_pub_crate)]
        let next = tokio::select! {
            v = shard.next_event() => v,
            _ = &mut shutdown_r => break,
        };
        let event = match next {
            Ok(event) => event,
            Err(source) => {
                tracing::warn!(?source, "error receiving event");
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
    state.db.close().await;
    Ok(())
}

async fn handle_message(mc: Message, state: AppState) {
    if mc.author.bot
        || !mc.guild_id.is_some_and(|v| v == state.guild_id)
        || state.xs.check_add(mc.author.id)
    {
        return;
    }
    let Some(member) = mc.member else {
        return;
    };
    #[allow(clippy::cast_possible_wrap)]
    let author_id_i64 = mc.author.id.get() as i64;
    let xp = match query!(
        "INSERT INTO levels(user, xp)
        VALUES (?, (abs(random()) % 10) + 15)
        ON CONFLICT DO UPDATE SET
        xp = xp + (abs(random()) % 10) + 15
        WHERE user = ?
        RETURNING xp",
        author_id_i64,
        author_id_i64
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(v) => v,
        Err(source) => {
            error!(?source, "failed to add xp to db");
            return;
        }
    }
    .xp;
    if member.roles.contains(&state.role_id) {
        return;
    }
    #[allow(clippy::cast_sign_loss)]
    if mee6::LevelInfo::new(xp as u64).level() > state.reward_level {
        if let Err(source) = state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await
        {
            error!(?source, "failed to add role to member");
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
    pub reward_level: u64,
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
        debug!("es: {self:?} ct: {:?}", Instant::now());
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
        debug!("es: {self:?} ct: {:?}", Instant::now());
        false
    }
}

impl Default for ExpiringSet {
    fn default() -> Self {
        Self::with_capacity(16)
    }
}
