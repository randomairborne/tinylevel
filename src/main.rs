#![warn(clippy::all, clippy::pedantic)]

use ahash::AHashMap;
use parking_lot::RwLock;
use sqlx::{
    query,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::{
    future::IntoFuture,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::oneshot::Receiver;
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};
use twilight_gateway::{CloseFrame, Event, Intents, Shard, ShardId};
use twilight_http::Client;
use twilight_model::{
    application::interaction::InteractionData,
    channel::{message::MessageFlags, Message},
    gateway::payload::incoming::InteractionCreate,
    guild::Permissions,
    http::interaction::InteractionResponse,
    id::{
        marker::{ApplicationMarker, GuildMarker, RoleMarker, UserMarker},
        Id,
    },
};
use twilight_util::builder::{
    command::CommandBuilder, embed::EmbedBuilder, InteractionResponseDataBuilder,
};

#[macro_use]
extern crate tracing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();
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
    let shard = Shard::new(ShardId::ONE, token.clone(), intents);
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
    let my_id = http.current_user_application().await?.model().await?.id;
    let state = AppState {
        http,
        db,
        xs,
        guild_id,
        role_id,
        activity_minutes,
        my_id,
    };
    let commands = [
        CommandBuilder::new(
            "Get Role Progress",
            "",
            twilight_model::application::command::CommandType::Message,
        )
        .default_member_permissions(Permissions::MANAGE_ROLES)
        .build(),
        CommandBuilder::new(
            "Get Role Progress",
            "",
            twilight_model::application::command::CommandType::User,
        )
        .default_member_permissions(Permissions::MANAGE_ROLES)
        .build(),
    ];
    state
        .http
        .interaction(state.my_id)
        .set_guild_commands(state.guild_id, &commands)
        .await?;
    let (shutdown_s, shutdown_r) = tokio::sync::oneshot::channel();
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
    event_loop(&state, shard, shutdown_r).await;
    info!("Shutting down!");
    Ok(())
}

async fn event_loop(state: &AppState, mut shard: Shard, mut shutdown_r: Receiver<()>) {
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
        match event {
            Event::ThreadCreate(thread) => {
                tokio::spawn(state.http.join_thread(thread.id).into_future());
            }
            Event::MessageCreate(mc) => {
                wrap_handle(handle_message(mc.0, state.clone())).await;
            }
            Event::InteractionCreate(ic) => {
                wrap_handle(handle_interaction(*ic, state.clone())).await;
            }
            _ => {}
        }
    }
    let _ = shard.close(CloseFrame::NORMAL).await;
}

#[allow(clippy::unused_async)]
async fn wrap_handle<F: IntoFuture<Output = Result<(), Error>> + Send + 'static>(fut: F)
where
    <F as IntoFuture>::IntoFuture: Send,
{
    tokio::spawn(async {
        if let Err(source) = fut.await {
            match source {
                Error::ValidateMessage(source) => error!(?source),
                Error::DiscordApi(source) => warn!(?source),
                Error::Sqlx(source) => error!(?source),
                Error::NoResolvedData | Error::NoAuthorResolvedData | Error::NoInteractionData => {
                    error!(?source);
                }
            }
        }
    });
}

async fn handle_interaction(ic: InteractionCreate, state: AppState) -> Result<(), Error> {
    let Some(InteractionData::ApplicationCommand(data)) = ic.data.as_ref() else {
        return Err(Error::NoInteractionData)?;
    };
    let id_m = match data.kind {
        twilight_model::application::command::CommandType::User => data.target_id.unwrap().cast(),
        twilight_model::application::command::CommandType::Message => {
            let Some(resolved) = data.resolved.as_ref() else {
                return Err(Error::NoResolvedData);
            };
            resolved
                .messages
                .get(&data.target_id.unwrap().cast())
                .ok_or(Error::NoAuthorResolvedData)?
                .author
                .id
        }
        _ => return Ok(()),
    };
    #[allow(clippy::cast_possible_wrap)]
    let id_i64 = id_m.get() as i64;
    let active_minutes = query!(
        "SELECT user, active_minutes FROM users WHERE user = $1",
        id_i64
    )
    .fetch_optional(&state.db)
    .await?
    .map_or(0, |v| v.active_minutes);
    let embed = EmbedBuilder::new()
        .description(format!(
            "user <@{id_m}> has been active for {active_minutes} minute{}",
            if active_minutes == 1 { "" } else { "s" }
        ))
        .build();
    let irc = InteractionResponseDataBuilder::new()
        .embeds([embed])
        .flags(MessageFlags::EPHEMERAL)
        .build();
    let response = InteractionResponse {
        data: Some(irc),
        kind: twilight_model::http::interaction::InteractionResponseType::ChannelMessageWithSource,
    };
    state
        .http
        .interaction(ic.application_id)
        .create_response(ic.id, &ic.token, &response)
        .await?;
    Ok(())
}

async fn handle_message(mc: Message, state: AppState) -> Result<(), Error> {
    if mc.author.bot
        || !mc.guild_id.is_some_and(|v| v == state.guild_id)
        || state.xs.check_add(mc.author.id)
    {
        debug!("skipped {}", mc.author.id);
        return Ok(());
    }
    let Some(member) = mc.member else {
        warn!("discord did not send a member object");
        return Ok(());
    };
    #[allow(clippy::cast_possible_wrap)]
    let author_id_i64 = mc.author.id.get() as i64;
    let active_minutes = query!(
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
    .await?
    .active_minutes;
    if member.roles.contains(&state.role_id) {
        debug!(
            "skipping {} because they already have the role",
            mc.author.id
        );
        return Ok(());
    }
    if active_minutes >= state.activity_minutes {
        state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct AppState {
    pub http: Arc<Client>,
    pub db: SqlitePool,
    pub xs: ExpiringSet,
    pub guild_id: Id<GuildMarker>,
    pub role_id: Id<RoleMarker>,
    pub activity_minutes: i64,
    pub my_id: Id<ApplicationMarker>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("twilight-validate error: {0}")]
    ValidateMessage(#[from] twilight_validate::message::MessageValidationError),
    #[error("twilight-http error: {0}")]
    DiscordApi(#[from] twilight_http::Error),
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("Discord did not send the resolved data section of the interaction!")]
    NoResolvedData,
    #[error("Discord did not send a message author matching the target of the interaction!")]
    NoAuthorResolvedData,
    #[error("Discord did not send the target of the interaction!")]
    NoInteractionData,
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
                return false;
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
