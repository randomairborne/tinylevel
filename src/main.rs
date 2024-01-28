#![warn(clippy::all, clippy::pedantic)]

use std::{
    future::IntoFuture,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::AHashMap;
use sqlx::{
    query,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use tokio::sync::{
    mpsc::{error::SendError as MpscSendError, Receiver as MpscReceiver, Sender as MpscSender},
    oneshot::{
        error::RecvError as OneshotRecvError, Receiver as OneshotReceiver, Sender as OneshotSender,
    },
};
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};
use twilight_gateway::{CloseFrame, Event, Intents, Shard, ShardId};
use twilight_http::Client;
use twilight_model::{
    application::{
        command::CommandType,
        interaction::{application_command::CommandData, InteractionData},
    },
    channel::{message::MessageFlags, Message},
    gateway::payload::incoming::{GuildAuditLogEntryCreate, InteractionCreate},
    guild::{audit_log::AuditLogEventType, Permissions},
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
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

const GET_PROGRESS_NAME: &str = "Get Role Progress";
const RESET_PROGRESS_NAME: &str = "Reset Role Progress";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let token = get_var("DATABASE_URL");
    let role_id: Id<RoleMarker> = parse_var("ROLE_ID");
    let guild_id: Id<GuildMarker> = parse_var("GUILD_ID");
    let activity_minutes: i64 = parse_var("ACTIVITY_MINUTES");
    let database_url = get_var("DATABASE_URL");
    let intents = Intents::GUILD_MESSAGES | Intents::GUILD_MODERATION;
    let shard = Shard::new(ShardId::ONE, token.clone(), intents);
    let db_opts = SqliteConnectOptions::from_str(&database_url)
        .expect("failed to parse DATABASE_URL")
        .create_if_missing(true)
        .optimize_on_close(true, None)
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
    let xs = Arc::new(ExpiringSet::new());
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
        CommandBuilder::new(GET_PROGRESS_NAME, "", CommandType::Message)
            .default_member_permissions(Permissions::MANAGE_ROLES)
            .build(),
        CommandBuilder::new(GET_PROGRESS_NAME, "", CommandType::User)
            .default_member_permissions(Permissions::MANAGE_ROLES)
            .build(),
        CommandBuilder::new(RESET_PROGRESS_NAME, "", CommandType::Message)
            .default_member_permissions(Permissions::MANAGE_ROLES)
            .build(),
        CommandBuilder::new(RESET_PROGRESS_NAME, "", CommandType::User)
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
    tokio::spawn(async move {
        vss::shutdown_signal().await;
        shutdown_s
            .send(())
            .expect("Failed to shut down, is the shutdown handler running?");
    });
    event_loop(&state, shard, shutdown_r).await;
    info!("Shutting down!");
    db_sd.close().await;
    Ok(())
}

async fn event_loop(state: &AppState, mut shard: Shard, mut shutdown_r: OneshotReceiver<()>) {
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
        let state = state.clone();
        match event {
            Event::MessageCreate(mc) => {
                wrap_handle(handle_message(mc.0, state)).await;
            }
            Event::InteractionCreate(ic) => {
                Box::pin(wrap_handle(handle_interaction(*ic, state))).await;
            }
            Event::GuildAuditLogEntryCreate(alec) => {
                wrap_handle(handle_audit_log(*alec, state)).await;
            }
            Event::GuildCreate(gc) => {
                wrap_handle(async move {
                    if gc.id != state.guild_id {
                        debug!("Leaving guild {} ({})", gc.name, gc.id);
                        state.http.leave_guild(gc.id).await?;
                    }
                    Ok(())
                })
                .await;
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
                Error::DiscordApi(source) => warn!(?source),
                _ => {
                    error!(?source);
                }
            }
        }
    });
}

async fn handle_interaction(ic: InteractionCreate, state: AppState) -> Result<(), Error> {
    let application_id = ic.application_id;
    let interaction_id = ic.id;
    let interaction_token = ic.token.clone();
    let interaction_response_data = command(ic, state.clone()).await.unwrap_or_else(|err| {
        InteractionResponseDataBuilder::new()
            .content(format!("Error: {err:?}"))
            .build()
    });
    let response = InteractionResponse {
        data: Some(interaction_response_data),
        kind: InteractionResponseType::ChannelMessageWithSource,
    };
    state
        .http
        .interaction(application_id)
        .create_response(interaction_id, &interaction_token, &response)
        .await?;
    Ok(())
}

async fn command(ic: InteractionCreate, state: AppState) -> Result<InteractionResponseData, Error> {
    let Some(InteractionData::ApplicationCommand(data)) = ic.data.as_ref() else {
        return Err(Error::NoInteractionData);
    };
    match data.name.as_str() {
        GET_PROGRESS_NAME => get_progress(data.as_ref(), state).await,
        RESET_PROGRESS_NAME => reset_progress(data.as_ref(), state).await,
        _ => Err(Error::UnknownCommand),
    }
}

async fn get_progress(
    data: &CommandData,
    state: AppState,
) -> Result<InteractionResponseData, Error> {
    let id = get_target(data)?;
    let id_i64 = id_to_db(id);
    let active_minutes = query!("SELECT id, active_minutes FROM users WHERE id = ?1", id_i64)
        .fetch_optional(&state.db)
        .await?
        .map_or(0, |v| v.active_minutes);
    let msg = format!(
        "user <@{id}> has been active for {active_minutes} minute{}",
        if active_minutes == 1 { "" } else { "s" }
    );
    let embed = EmbedBuilder::new().description(msg).build();
    let ird = InteractionResponseDataBuilder::new()
        .embeds([embed])
        .flags(MessageFlags::EPHEMERAL)
        .build();
    Ok(ird)
}

async fn reset_progress(
    data: &CommandData,
    state: AppState,
) -> Result<InteractionResponseData, Error> {
    let id = get_target(data)?;
    let id_i64 = id_to_db(id);
    let db_deleted = query!("DELETE FROM users WHERE id = ?1", id_i64)
        .execute(&state.db)
        .await
        .is_ok();
    let guild_id = data.guild_id.ok_or(Error::NoGuildId)?;
    let role_removed = state
        .http
        .remove_guild_member_role(guild_id, id, state.role_id)
        .await
        .is_ok();
    let msg = match (db_deleted, role_removed) {
        (true, true) => format!("user <@{id}> has been reset"),
        (false, true) => {
            format!("user <@{id}>'s role was removed, but their message count could not be reset")
        }
        (true, false) => {
            format!("user <@{id}> has been reset, but their role could not be removed")
        }
        (false, false) => format!("user <@{id}> reset failed"),
    };
    let embed = EmbedBuilder::new().description(msg).build();
    let ird = InteractionResponseDataBuilder::new()
        .embeds([embed])
        .flags(MessageFlags::EPHEMERAL)
        .build();
    Ok(ird)
}

fn get_target(data: &CommandData) -> Result<Id<UserMarker>, Error> {
    let id = match data.kind {
        CommandType::User => data.target_id.unwrap().cast(),
        CommandType::Message => {
            data.resolved
                .as_ref()
                .ok_or(Error::NoResolvedData)?
                .messages
                .get(&data.target_id.unwrap().cast())
                .ok_or(Error::NoAuthorResolvedData)?
                .author
                .id
        }
        _ => return Err(Error::UnknownCommandType),
    };
    Ok(id)
}

async fn handle_message(mc: Message, state: AppState) -> Result<(), Error> {
    if mc.author.bot
        || !mc.guild_id.is_some_and(|v| v == state.guild_id)
        || state.xs.check_add(mc.author.id).await?
    {
        debug!("skipped {}", mc.author.id);
        return Ok(());
    }
    let member = mc.member.ok_or(Error::NoPartialMember)?;
    let db_id = id_to_db(mc.author.id);
    let active_minutes = query!(
        "INSERT INTO users
        (id, active_minutes)
        VALUES (?1, 1)
        ON CONFLICT DO UPDATE SET
        active_minutes = active_minutes + 1
        WHERE id = ?1
        RETURNING active_minutes",
        db_id
    )
    .fetch_one(&state.db)
    .await?
    .active_minutes;
    if member.roles.contains(&state.role_id) {
        debug!(
            "skipping {} because they already have the role",
            mc.author.id
        );
    } else if active_minutes >= state.activity_minutes {
        state
            .http
            .add_guild_member_role(state.guild_id, mc.author.id, state.role_id)
            .await?;
    }
    Ok(())
}

async fn handle_audit_log(
    audit_log: GuildAuditLogEntryCreate,
    state: AppState,
) -> Result<(), Error> {
    trace!(?audit_log, "Got audit log event");
    if let Some(target) = audit_log.target_id {
        match audit_log.action_type {
            AuditLogEventType::MemberBanAdd | AuditLogEventType::MemberKick => {
                delete_user(id_to_db(target), &state.db).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn delete_user(target_i64: i64, db: &SqlitePool) -> Result<(), Error> {
    query!("DELETE FROM users WHERE id = ?1", target_i64)
        .execute(db)
        .await?;
    Ok(())
}

fn get_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} required in the environment"))
}

fn parse_var<T>(name: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Debug,
{
    get_var(name)
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a valid {}", std::any::type_name::<T>()))
}

#[inline]
fn id_to_db<T>(id: Id<T>) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    let id = id.get() as i64;
    id
}

#[derive(Clone)]
pub struct AppState {
    pub http: Arc<Client>,
    pub db: SqlitePool,
    pub xs: Arc<ExpiringSet>,
    pub guild_id: Id<GuildMarker>,
    pub role_id: Id<RoleMarker>,
    pub activity_minutes: i64,
    pub my_id: Id<ApplicationMarker>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("twilight-validate error")]
    ValidateMessage(#[from] twilight_validate::message::MessageValidationError),
    #[error("twilight-http error")]
    DiscordApi(#[from] twilight_http::Error),
    #[error("sqlx error")]
    Sqlx(#[from] sqlx::Error),
    #[error("mpsc error")]
    MpscSend(#[from] MpscSendError<ExpiryRequest>),
    #[error("oneshot error")]
    OneshotRecv(#[from] OneshotRecvError),
    #[error("Discord did not send the resolved data section of the interaction!")]
    NoResolvedData,
    #[error("Discord did not send a message author matching the target of the interaction!")]
    NoAuthorResolvedData,
    #[error("Discord did not send the target of the interaction!")]
    NoInteractionData,
    #[error("Discord did not send the partial member for this message!")]
    NoPartialMember,
    #[error("Discord did not send guild ID!")]
    NoGuildId,
    #[error("There is no command with this name")]
    UnknownCommand,
    #[error("There is a command with this name, but not with this type!")]
    UnknownCommandType,
}

#[derive(Debug)]
pub struct ExpiringSet {
    requests: MpscSender<ExpiryRequest>,
}

pub struct ExpiryRequest {
    pub returner: OneshotSender<bool>,
    pub user: Id<UserMarker>,
}

type ExpiringSetMap = AHashMap<Id<UserMarker>, Instant>;

impl ExpiringSet {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        std::thread::spawn(move || Self::task(rx));
        Self { requests: tx }
    }

    fn task(mut rx: MpscReceiver<ExpiryRequest>) {
        let mut last_vac = Instant::now();
        let mut set: ExpiringSetMap = AHashMap::with_capacity(16);
        while let Some(er) = rx.blocking_recv() {
            let ExpiryRequest { returner, user } = er;
            if let Some(v) = set.get(&user) {
                if v.elapsed() > Duration::from_secs(60) {
                    let _ = returner.send(true);
                    continue;
                }
            }
            if set.contains_key(&user) {
                let _ = returner.send(true);
                continue;
            }
            if last_vac.elapsed() > Duration::from_secs(60) {
                set.retain(|_id, expiry| *expiry < Instant::now());
                let shrink_target = set.len() + 1;
                set.shrink_to(shrink_target);
                last_vac = Instant::now();
            }
            set.insert(user, Instant::now());
            let _ = returner.send(false);
        }
    }

    /// Checks if the user is in the set. If not, adds them.
    ///
    /// # Errors
    /// If the sender is misused, then this can error
    pub async fn check_add(&self, user: Id<UserMarker>) -> Result<bool, Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let request = ExpiryRequest { returner: tx, user };
        self.requests.send(request).await?;
        Ok(rx.await?)
    }
}

impl Default for ExpiringSet {
    fn default() -> Self {
        Self::new()
    }
}
