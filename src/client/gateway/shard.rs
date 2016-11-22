use serde_json::builder::ObjectBuilder;
use std::io::Write;
use std::net::Shutdown;
use std::sync::mpsc::{self, Sender as MpscSender};
use std::thread::{self, Builder as ThreadBuilder};
use std::time::Duration as StdDuration;
use std::mem;
use super::super::login_type::LoginType;
use super::super::Client;
use super::{GatewayError, GatewayStatus, prep};
use websocket::client::{Client as WsClient, Sender, Receiver};
use websocket::message::Message as WsMessage;
use websocket::stream::WebSocketStream;
use websocket::ws::sender::Sender as WsSender;
use ::constants::OpCode;
use ::internal::prelude::*;
use ::internal::ws_impl::{ReceiverExt, SenderExt};
use ::model::{
    ChannelId,
    Event,
    Game,
    GatewayEvent,
    GuildId,
    OnlineStatus,
    ReadyEvent,
};

#[cfg(feature="voice")]
use ::ext::voice::Manager as VoiceManager;

type CurrentPresence = (Option<Game>, OnlineStatus, bool);

/// A Shard is a higher-level handler for a websocket connection to Discord's
/// gateway. The shard allows for sending and receiving messages over the
/// websocket, such as setting the active game, reconnecting, syncing guilds,
/// and more.
///
/// Refer to the [module-level documentation][module docs] for information on
/// effectively using multiple shards, if you need to.
///
/// # Stand-alone shards
///
/// You may instantiate a shard yourself - decoupled from the [`Client`] - if
/// you need to. For most use cases, you will not need to do this, and you can
/// leave the client to do it.
///
/// This can be done by passing in the required parameters to [`new`]. You can
/// then manually handle the shard yourself and receive events via
/// [`receive`].
///
/// **Note**: You _really_ do not need to do this. Just call one of the
/// appropriate methods on the [`Client`].
///
/// # Examples
///
/// See the documentation for [`new`] on how to use this.
///
/// [`Client`]: struct.Client.html
/// [`new`]: #method.new
/// [`receive`]: #method.receive
/// [docs]: https://discordapp.com/developers/docs/topics/gateway#sharding
/// [module docs]: index.html#sharding
pub struct Shard {
    current_presence: CurrentPresence,
    keepalive_channel: MpscSender<GatewayStatus>,
    last_sequence: u64,
    login_type: LoginType,
    session_id: Option<String>,
    shard_info: Option<[u8; 2]>,
    token: String,
    ws_url: String,
    #[cfg(feature = "voice")]
    pub manager: VoiceManager,
}

impl Shard {
    /// Instantiates a new instance of a Shard, bypassing the client.
    ///
    /// **Note**: You should likely never need to do this yourself.
    ///
    /// # Examples
    ///
    /// Instantiating a new Shard manually for a bot with no shards, and
    /// then listening for events:
    ///
    /// ```rust,ignore
    /// use serenity::client::gateway::Shard;
    /// use serenity::client::{LoginType, http};
    /// use std::env;
    ///
    /// let token = env::var("DISCORD_BOT_TOKEN").expect("Token in environment");
    /// // retrieve the gateway response, which contains the URL to connect to
    /// let gateway = http::get_gateway().expect("Valid gateway response").url;
    /// let shard = Shard::new(&gateway, &token, None, LoginType::Bot)
    ///     .expect("Working shard");
    ///
    /// // at this point, you can create a `loop`, and receive events and match
    /// // their variants
    /// ```
    pub fn new(base_url: &str,
               token: &str,
               shard_info: Option<[u8; 2]>,
               login_type: LoginType)
               -> Result<(Shard, ReadyEvent, Receiver<WebSocketStream>)> {
        let url = try!(prep::build_gateway_url(base_url));

        let response = try!(try!(WsClient::connect(url)).send());
        try!(response.validate());

        let (mut sender, mut receiver) = response.begin().split();

        let identification = prep::identify(token, shard_info);
        try!(sender.send_json(&identification));

        let heartbeat_interval = match try!(receiver.recv_json(GatewayEvent::decode)) {
            GatewayEvent::Hello(interval) => interval,
            other => {
                debug!("Unexpected event during connection start: {:?}", other);

                return Err(Error::Gateway(GatewayError::ExpectedHello));
            },
        };

        let (tx, rx) = mpsc::channel();
        let thread_name = match shard_info {
            Some(info) => format!("serenity keepalive [shard {}/{}]",
                                  info[0],
                                  info[1] - 1),
            None => "serenity keepalive [unsharded]".to_owned(),
        };
        try!(ThreadBuilder::new()
            .name(thread_name)
            .spawn(move || prep::keepalive(heartbeat_interval, sender, rx)));

        // Parse READY
        let event = try!(receiver.recv_json(GatewayEvent::decode));
        let (ready, sequence) = try!(prep::parse_ready(event,
                                                 &tx,
                                                 &mut receiver,
                                                 identification));

        Ok((feature_voice! {{
            Shard {
                current_presence: (None, OnlineStatus::Online, false),
                keepalive_channel: tx.clone(),
                last_sequence: sequence,
                login_type: login_type,
                token: token.to_owned(),
                session_id: Some(ready.ready.session_id.clone()),
                shard_info: shard_info,
                ws_url: base_url.to_owned(),
                manager: VoiceManager::new(tx, ready.ready.user.id.0),
            }
        } else {
            Shard {
                current_presence: (None, OnlineStatus::Online, false),
                keepalive_channel: tx.clone(),
                last_sequence: sequence,
                login_type: login_type,
                token: token.to_owned(),
                session_id: Some(ready.ready.session_id.clone()),
                shard_info: shard_info,
                ws_url: base_url.to_owned(),
            }
        }}, ready, receiver))
    }

    pub fn shard_info(&self) -> Option<[u8; 2]> {
        self.shard_info
    }

    /// Sets whether the current user is afk. This helps Discord determine where
    /// to send notifications.
    ///
    /// Other presence settings are maintained.
    pub fn set_afk(&mut self, afk: bool) {
        self.current_presence.2 = afk;

        self.update_presence();
    }

    /// Sets the user's current game, if any.
    ///
    /// Other presence settings are maintained.
    pub fn set_game(&mut self, game: Option<Game>) {
        self.current_presence.0 = game;

        self.update_presence();
    }

    /// Sets the user's current online status.
    ///
    /// Note that [`Offline`] is not a valid presence, so it is automatically
    /// converted to [`Invisible`].
    ///
    /// Other presence settings are maintained.
    pub fn set_status(&mut self, online_status: OnlineStatus) {
        self.current_presence.1 = match online_status {
            OnlineStatus::Offline => OnlineStatus::Invisible,
            other => other,
        };

        self.update_presence();
    }

    /// Sets the user's full presence information.
    ///
    /// Consider using the individual setters if you only need to modify one of
    /// these.
    ///
    /// # Examples
    ///
    /// Set the current user as playing `"Heroes of the Storm"`, being online,
    /// and not being afk:
    ///
    /// ```rust,ignore
    /// use serenity::model::{Game, OnlineStatus};
    ///
    /// // assuming you are in a context
    ///
    /// context.shard.lock()
    ///     .unwrap()
    ///     .set_presence(Game::playing("Heroes of the Storm"),
    ///                   OnlineStatus::Online,
    ///                   false);
    /// ```
    pub fn set_presence(&mut self,
                        game: Option<Game>,
                        status: OnlineStatus,
                        afk: bool) {
        let status = match status {
            OnlineStatus::Offline => OnlineStatus::Invisible,
            other => other,
        };

        self.current_presence = (game, status, afk);

        self.update_presence();
    }

    pub fn handle_event(&mut self,
                        event: Result<GatewayEvent>,
                        mut receiver: &mut Receiver<WebSocketStream>)
                        -> Result<Option<(Event, Option<Receiver<WebSocketStream>>)>> {
        match event {
            Ok(GatewayEvent::Dispatch(sequence, event)) => {
                let status = GatewayStatus::Sequence(sequence);
                let _ = self.keepalive_channel.send(status);

                self.handle_dispatch(&event);

                Ok(Some((event, None)))
            },
            Ok(GatewayEvent::Heartbeat(sequence)) => {
                let map = ObjectBuilder::new()
                    .insert("d", sequence)
                    .insert("op", OpCode::Heartbeat.num())
                    .build();
                let _ = self.keepalive_channel.send(GatewayStatus::SendMessage(map));

                Ok(None)
            },
            Ok(GatewayEvent::HeartbeatAck) => {
                Ok(None)
            },
            Ok(GatewayEvent::Hello(interval)) => {
                let _ = self.keepalive_channel.send(GatewayStatus::ChangeInterval(interval));

                Ok(None)
            },
            Ok(GatewayEvent::InvalidateSession) => {
                self.session_id = None;

                let identification = prep::identify(&self.token, self.shard_info);

                let status = GatewayStatus::SendMessage(identification);

                let _ = self.keepalive_channel.send(status);

                Ok(None)
            },
            Ok(GatewayEvent::Reconnect) => {
                self.reconnect(receiver).map(|(ev, rec)| Some((ev, Some(rec))))
            },
            Err(Error::Gateway(GatewayError::Closed(num, message))) => {
                warn!("Closing with {:?}: {:?}", num, message);

                // Attempt to resume if the following was not received:
                //
                // - 1000: Close.
                //
                // Otherwise, fallback to reconnecting.
                if num != Some(1000) {
                    if let Some(session_id) = self.session_id.clone() {
                        match self.resume(session_id, receiver) {
                            Ok((ev, rec)) => return Ok(Some((ev, Some(rec)))),
                            Err(why) => debug!("Err resuming: {:?}", why),
                        }
                    }
                }

                self.reconnect(receiver).map(|(ev, rec)| Some((ev, Some(rec))))
            },
            Err(Error::WebSocket(why)) => {
                warn!("Websocket error: {:?}", why);
                info!("Reconnecting");

                // Attempt to resume if the following was not received:
                //
                // - InvalidateSession.
                //
                // Otherwise, fallback to reconnecting.
                if let Some(session_id) = self.session_id.clone() {
                    match self.resume(session_id, &mut receiver) {
                        Ok((ev, rec)) => return Ok(Some((ev, Some(rec)))),
                        Err(why) => debug!("Err resuming: {:?}", why),
                    }
                }

                self.reconnect(receiver).map(|(ev, rec)| Some((ev, Some(rec))))
            },
            Err(error) => Err(error),
        }
    }

    pub fn shutdown(&mut self, receiver: &mut Receiver<WebSocketStream>)
        -> Result<()> {
        let stream = receiver.get_mut().get_mut();

        {
            let mut sender = Sender::new(stream.by_ref(), true);
            let message = WsMessage::close_because(1000, "");

            try!(sender.send_message(&message));
        }

        try!(stream.flush());
        try!(stream.shutdown(Shutdown::Both));

        Ok(())
    }

    pub fn sync_calls(&self, channels: &[ChannelId]) {
        for &channel in channels {
            let msg = ObjectBuilder::new()
                .insert("op", OpCode::SyncCall.num())
                .insert_object("d", |obj| obj
                    .insert("channel_id", channel.0)
                )
                .build();

            let _ = self.keepalive_channel.send(GatewayStatus::SendMessage(msg));
        }
    }

    pub fn sync_guilds(&self, guild_ids: &[GuildId]) {
        let msg = ObjectBuilder::new()
            .insert("op", OpCode::SyncGuild.num())
            .insert_array("d", |a| guild_ids.iter().fold(a, |a, s| a.push(s.0)))
            .build();

        let _ = self.keepalive_channel.send(GatewayStatus::SendMessage(msg));
    }

    fn handle_dispatch(&mut self, event: &Event) {
        if let Event::Resumed(ref ev) = *event {
            let status = GatewayStatus::ChangeInterval(ev.heartbeat_interval);

            let _ = self.keepalive_channel.send(status);
        }

        feature_voice_enabled! {{
            if let Event::VoiceStateUpdate(ref update) = *event {
                if let Some(guild_id) = update.guild_id {
                    if let Some(handler) = self.manager.get(guild_id) {
                        handler.update_state(&update.voice_state);
                    }
                }
            }

            if let Event::VoiceServerUpdate(ref update) = *event {
                if let Some(guild_id) = update.guild_id {
                    if let Some(handler) = self.manager.get(guild_id) {
                        handler.update_server(&update.endpoint, &update.token);
                    }
                }
            }
        }}
    }

    fn reconnect(&mut self, mut receiver: &mut Receiver<WebSocketStream>) -> Result<(Event, Receiver<WebSocketStream>)> {
        debug!("Reconnecting");

        // Take a few attempts at reconnecting; otherwise fall back to
        // re-instantiating the connection.
        for _ in 0..3 {
            let shard = Shard::new(&self.ws_url,
                                   &self.token,
                                   self.shard_info,
                                   self.login_type);

            if let Ok((shard, ready, receiver_new)) = shard {
                try!(mem::replace(self, shard).shutdown(&mut receiver));

                self.session_id = Some(ready.ready.session_id.clone());

                return Ok((Event::Ready(ready), receiver_new));
            }

            thread::sleep(StdDuration::from_secs(1));
        }

        // If all else fails: get a new endpoint.
        //
        // A bit of complexity here: instantiate a temporary instance of a
        // Client. This client _does not_ replace the current client(s) that the
        // user has. This client will then connect to gateway. This new
        // shard will be used to replace _this_ shard.
        let (shard, ready, receiver_new) = {
            let mut client = Client::login_raw(&self.token.clone(),
                                               self.login_type);

            try!(client.boot_shard(self.shard_info))
        };

        // Replace this shard with a new one, and shutdown the now-old
        // shard.
        try!(mem::replace(self, shard).shutdown(&mut receiver));

        self.session_id = Some(ready.ready.session_id.clone());

        Ok((Event::Ready(ready), receiver_new))
    }

    fn resume(&mut self, session_id: String, receiver: &mut Receiver<WebSocketStream>)
        -> Result<(Event, Receiver<WebSocketStream>)> {
        try!(receiver.get_mut().get_mut().shutdown(Shutdown::Both));
        let url = try!(prep::build_gateway_url(&self.ws_url));

        let response = try!(try!(WsClient::connect(url)).send());
        try!(response.validate());

        let (mut sender, mut receiver) = response.begin().split();

        try!(sender.send_json(&ObjectBuilder::new()
            .insert_object("d", |o| o
                .insert("session_id", session_id)
                .insert("seq", self.last_sequence)
                .insert("token", &self.token)
            )
            .insert("op", OpCode::Resume.num())
            .build()));

        let first_event;

        loop {
            match try!(receiver.recv_json(GatewayEvent::decode)) {
                GatewayEvent::Dispatch(seq, event) => {
                    if let Event::Ready(ref event) = event {
                        self.session_id = Some(event.ready.session_id.clone());
                    }

                    self.last_sequence = seq;
                    first_event = event;

                    break;
                },
                GatewayEvent::InvalidateSession => {
                    try!(sender.send_json(&prep::identify(&self.token, self.shard_info)));
                },
                other => {
                    debug!("Unexpected event: {:?}", other);

                    return Err(Error::Gateway(GatewayError::InvalidHandshake));
                },
            }
        }

        let _ = self.keepalive_channel.send(GatewayStatus::ChangeSender(sender));

        Ok((first_event, receiver))
    }

    fn update_presence(&self) {
        let (ref game, status, afk) = self.current_presence;

        let msg = ObjectBuilder::new()
            .insert("op", OpCode::StatusUpdate.num())
            .insert_object("d", move |mut object| {
                object = object.insert("since", 0)
                    .insert("afk", afk)
                    .insert("status", status.name());

                match game.as_ref() {
                    Some(game) => {
                        object.insert_object("game", move |o| o
                            .insert("name", &game.name))
                    },
                    None => object.insert("game", Value::Null),
                }
            })
            .build();

        let _ = self.keepalive_channel.send(GatewayStatus::SendMessage(msg));
    }
}
