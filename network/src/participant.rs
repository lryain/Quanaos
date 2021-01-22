#[cfg(feature = "metrics")]
use crate::metrics::NetworkMetrics;
use crate::{
    api::{ParticipantError, Stream},
    channel::{Protocols, RecvProtocols, SendProtocols},
};
use futures_util::{FutureExt, StreamExt};
use network_protocol::{
    Bandwidth, Cid, MessageBuffer, Pid, Prio, Promises, ProtocolEvent, RecvProtocol, SendProtocol,
    Sid,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::{mpsc, oneshot, Mutex, RwLock},
    task::JoinHandle,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;

pub(crate) type A2bStreamOpen = (Prio, Promises, Bandwidth, oneshot::Sender<Stream>);
pub(crate) type S2bCreateChannel = (Cid, Sid, Protocols, oneshot::Sender<()>);
pub(crate) type S2bShutdownBparticipant = (Duration, oneshot::Sender<Result<(), ParticipantError>>);
pub(crate) type B2sPrioStatistic = (Pid, u64, u64);

#[derive(Debug)]
struct ChannelInfo {
    cid: Cid,
    cid_string: String, //optimisationmetrics
}

#[derive(Debug)]
struct StreamInfo {
    prio: Prio,
    promises: Promises,
    send_closed: Arc<AtomicBool>,
    b2a_msg_recv_s: Mutex<async_channel::Sender<MessageBuffer>>,
}

#[derive(Debug)]
struct ControlChannels {
    a2b_open_stream_r: mpsc::UnboundedReceiver<A2bStreamOpen>,
    b2a_stream_opened_s: mpsc::UnboundedSender<Stream>,
    s2b_create_channel_r: mpsc::UnboundedReceiver<S2bCreateChannel>,
    s2b_shutdown_bparticipant_r: oneshot::Receiver<S2bShutdownBparticipant>, /* own */
}

#[derive(Debug)]
struct ShutdownInfo {
    b2b_close_stream_opened_sender_s: Option<oneshot::Sender<()>>,
    error: Option<ParticipantError>,
}

#[derive(Debug)]
pub struct BParticipant {
    remote_pid: Pid,
    remote_pid_string: String, //optimisation
    offset_sid: Sid,
    channels: Arc<RwLock<HashMap<Cid, Mutex<ChannelInfo>>>>,
    streams: RwLock<HashMap<Sid, StreamInfo>>,
    run_channels: Option<ControlChannels>,
    shutdown_barrier: AtomicI32,
    #[cfg(feature = "metrics")]
    metrics: Arc<NetworkMetrics>,
    no_channel_error_info: RwLock<(Instant, u64)>,
}

impl BParticipant {
    // We use integer instead of Barrier to not block mgr from freeing at the end
    const BARR_CHANNEL: i32 = 1;
    const BARR_RECV: i32 = 4;
    const BARR_SEND: i32 = 2;
    const TICK_TIME: Duration = Duration::from_millis(Self::TICK_TIME_MS);
    const TICK_TIME_MS: u64 = 10;

    #[allow(clippy::type_complexity)]
    pub(crate) fn new(
        remote_pid: Pid,
        offset_sid: Sid,
        #[cfg(feature = "metrics")] metrics: Arc<NetworkMetrics>,
    ) -> (
        Self,
        mpsc::UnboundedSender<A2bStreamOpen>,
        mpsc::UnboundedReceiver<Stream>,
        mpsc::UnboundedSender<S2bCreateChannel>,
        oneshot::Sender<S2bShutdownBparticipant>,
    ) {
        let (a2b_open_stream_s, a2b_open_stream_r) = mpsc::unbounded_channel::<A2bStreamOpen>();
        let (b2a_stream_opened_s, b2a_stream_opened_r) = mpsc::unbounded_channel::<Stream>();
        let (s2b_shutdown_bparticipant_s, s2b_shutdown_bparticipant_r) = oneshot::channel();
        let (s2b_create_channel_s, s2b_create_channel_r) = mpsc::unbounded_channel();

        let run_channels = Some(ControlChannels {
            a2b_open_stream_r,
            b2a_stream_opened_s,
            s2b_create_channel_r,
            s2b_shutdown_bparticipant_r,
        });

        (
            Self {
                remote_pid,
                remote_pid_string: remote_pid.to_string(),
                offset_sid,
                channels: Arc::new(RwLock::new(HashMap::new())),
                streams: RwLock::new(HashMap::new()),
                shutdown_barrier: AtomicI32::new(
                    Self::BARR_CHANNEL + Self::BARR_SEND + Self::BARR_RECV,
                ),
                run_channels,
                #[cfg(feature = "metrics")]
                metrics,
                no_channel_error_info: RwLock::new((Instant::now(), 0)),
            },
            a2b_open_stream_s,
            b2a_stream_opened_r,
            s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
        )
    }

    pub async fn run(mut self, b2s_prio_statistic_s: mpsc::UnboundedSender<B2sPrioStatistic>) {
        let (b2b_add_send_protocol_s, b2b_add_send_protocol_r) =
            mpsc::unbounded_channel::<(Cid, SendProtocols)>();
        let (b2b_add_recv_protocol_s, b2b_add_recv_protocol_r) =
            mpsc::unbounded_channel::<(Cid, RecvProtocols)>();
        let (b2b_close_send_protocol_s, b2b_close_send_protocol_r) =
            async_channel::unbounded::<Cid>();
        let (b2b_force_close_recv_protocol_s, b2b_force_close_recv_protocol_r) =
            async_channel::unbounded::<Cid>();

        let (a2b_close_stream_s, a2b_close_stream_r) = mpsc::unbounded_channel::<Sid>();
        const STREAM_BOUND: usize = 10_000;
        let (a2b_msg_s, a2b_msg_r) =
            crossbeam_channel::bounded::<(Sid, Arc<MessageBuffer>)>(STREAM_BOUND);

        let run_channels = self.run_channels.take().unwrap();
        tokio::join!(
            self.send_mgr(
                run_channels.a2b_open_stream_r,
                a2b_close_stream_r,
                a2b_msg_r,
                b2b_add_send_protocol_r,
                b2b_close_send_protocol_r,
                b2s_prio_statistic_s,
                a2b_msg_s.clone(),          //self
                a2b_close_stream_s.clone(), //self
            ),
            self.recv_mgr(
                run_channels.b2a_stream_opened_s,
                b2b_add_recv_protocol_r,
                b2b_force_close_recv_protocol_r,
                b2b_close_send_protocol_s.clone(),
                a2b_msg_s.clone(),          //self
                a2b_close_stream_s.clone(), //self
            ),
            self.create_channel_mgr(
                run_channels.s2b_create_channel_r,
                b2b_add_send_protocol_s,
                b2b_add_recv_protocol_s,
            ),
            self.participant_shutdown_mgr(
                run_channels.s2b_shutdown_bparticipant_r,
                b2b_close_send_protocol_s.clone(),
                b2b_force_close_recv_protocol_s,
            ),
        );
    }

    //TODO: local stream_cid: HashMap<Sid, Cid> to know the respective protocol
    async fn send_mgr(
        &self,
        mut a2b_open_stream_r: mpsc::UnboundedReceiver<A2bStreamOpen>,
        mut a2b_close_stream_r: mpsc::UnboundedReceiver<Sid>,
        a2b_msg_r: crossbeam_channel::Receiver<(Sid, Arc<MessageBuffer>)>,
        mut b2b_add_protocol_r: mpsc::UnboundedReceiver<(Cid, SendProtocols)>,
        b2b_close_send_protocol_r: async_channel::Receiver<Cid>,
        _b2s_prio_statistic_s: mpsc::UnboundedSender<B2sPrioStatistic>,
        a2b_msg_s: crossbeam_channel::Sender<(Sid, Arc<MessageBuffer>)>,
        a2b_close_stream_s: mpsc::UnboundedSender<Sid>,
    ) {
        let mut send_protocols: HashMap<Cid, SendProtocols> = HashMap::new();
        let mut interval = tokio::time::interval(Self::TICK_TIME);
        let mut stream_ids = self.offset_sid;
        trace!("workaround, activly wait for first protocol");
        b2b_add_protocol_r
            .recv()
            .await
            .map(|(c, p)| send_protocols.insert(c, p));
        trace!("Start send_mgr");
        loop {
            let (open, close, _, addp, remp) = select!(
                next = a2b_open_stream_r.recv().fuse() => (Some(next), None, None, None, None),
                next = a2b_close_stream_r.recv().fuse() => (None, Some(next), None, None, None),
                _ = interval.tick() => (None, None, Some(()), None, None),
                next = b2b_add_protocol_r.recv().fuse() => (None, None, None, Some(next), None),
                next = b2b_close_send_protocol_r.recv().fuse() => (None, None, None, None, Some(next)),
            );

            trace!(?open, ?close, ?addp, ?remp, "foobar");

            addp.flatten().map(|(c, p)| send_protocols.insert(c, p));
            match remp {
                Some(Ok(cid)) => {
                    trace!(?cid, "remove send protocol");
                    match send_protocols.remove(&cid) {
                        Some(mut prot) => {
                            trace!("blocking flush");
                            let _ = prot.flush(u64::MAX, Duration::from_secs(1)).await;
                            trace!("shutdown prot");
                            let _ = prot.send(ProtocolEvent::Shutdown).await;
                        },
                        None => trace!("tried to remove protocol twice"),
                    };
                    if send_protocols.is_empty() {
                        break;
                    }
                },
                _ => (),
            };

            let cid = 0;
            let active = match send_protocols.get_mut(&cid) {
                Some(a) => a,
                None => {
                    warn!("no channel arrg");
                    continue;
                },
            };

            let active_err = async {
                if let Some(Some((prio, promises, guaranteed_bandwidth, return_s))) = open {
                    trace!(?stream_ids, "openuing some new stream");
                    let sid = stream_ids;
                    stream_ids += Sid::from(1);
                    let stream = self
                        .create_stream(
                            sid,
                            prio,
                            promises,
                            guaranteed_bandwidth,
                            &a2b_msg_s,
                            &a2b_close_stream_s,
                        )
                        .await;

                    let event = ProtocolEvent::OpenStream {
                        sid,
                        prio,
                        promises,
                        guaranteed_bandwidth,
                    };

                    return_s.send(stream).unwrap();
                    active.send(event).await?;
                }

                // get all messages and assign it to a channel
                for (sid, buffer) in a2b_msg_r.try_iter() {
                    warn!(?sid, "sending!");
                    active
                        .send(ProtocolEvent::Message {
                            buffer,
                            mid: 0u64,
                            sid,
                        })
                        .await?
                }

                if let Some(Some(sid)) = close {
                    warn!(?sid, "delete_stream!");
                    self.delete_stream(sid).await;
                    // Fire&Forget the protocol will take care to verify that this Frame is delayed
                    // till the last msg was received!
                    active.send(ProtocolEvent::CloseStream { sid }).await?;
                }

                warn!("flush!");
                active
                    .flush(1_000_000, Duration::from_secs(1) /* TODO */)
                    .await?; //this actually blocks, so we cant set streams whilte it.
                let r: Result<(), network_protocol::ProtocolError> = Ok(());
                r
            }
            .await;
            if let Err(e) = active_err {
                info!(?cid, ?e, "send protocol failed, shutting down channel");
                // remote recv will now fail, which will trigger remote send which will trigger
                // recv
                send_protocols.remove(&cid).unwrap();
            }
        }
        trace!("Stop send_mgr");
        self.shutdown_barrier
            .fetch_sub(Self::BARR_SEND, Ordering::Relaxed);
    }

    async fn recv_mgr(
        &self,
        b2a_stream_opened_s: mpsc::UnboundedSender<Stream>,
        mut b2b_add_protocol_r: mpsc::UnboundedReceiver<(Cid, RecvProtocols)>,
        b2b_force_close_recv_protocol_r: async_channel::Receiver<Cid>,
        b2b_close_send_protocol_s: async_channel::Sender<Cid>,
        a2b_msg_s: crossbeam_channel::Sender<(Sid, Arc<MessageBuffer>)>,
        a2b_close_stream_s: mpsc::UnboundedSender<Sid>,
    ) {
        let mut recv_protocols: HashMap<Cid, JoinHandle<()>> = HashMap::new();
        // we should be able to directly await futures imo
        let (hacky_recv_s, mut hacky_recv_r) = mpsc::unbounded_channel();

        let retrigger = |cid: Cid, mut p: RecvProtocols, map: &mut HashMap<_, _>| {
            let hacky_recv_s = hacky_recv_s.clone();
            let handle = tokio::spawn(async move {
                let cid = cid;
                let r = p.recv().await;
                let _ = hacky_recv_s.send((cid, r, p)); // ignoring failed
            });
            map.insert(cid, handle);
        };

        let remove_c = |recv_protocols: &mut HashMap<Cid, JoinHandle<()>>, cid: &Cid| {
            match recv_protocols.remove(&cid) {
                Some(h) => h.abort(),
                None => trace!("tried to remove protocol twice"),
            };
            recv_protocols.is_empty()
        };

        trace!("Start recv_mgr");
        loop {
            let (event, addp, remp) = select!(
                next = hacky_recv_r.recv().fuse() => (Some(next), None, None),
                Some(next) = b2b_add_protocol_r.recv().fuse() => (None, Some(next), None),
                next = b2b_force_close_recv_protocol_r.recv().fuse() => (None, None, Some(next)),
            );

            addp.map(|(cid, p)| {
                retrigger(cid, p, &mut recv_protocols);
            });
            if let Some(Ok(cid)) = remp {
                // no need to stop the send_mgr here as it has been canceled before
                if remove_c(&mut recv_protocols, &cid) {
                    break;
                }
            };

            warn!(?event, "recv event!");
            if let Some(Some((cid, r, p))) = event {
                match r {
                    Ok(ProtocolEvent::OpenStream {
                        sid,
                        prio,
                        promises,
                        guaranteed_bandwidth,
                    }) => {
                        trace!(?sid, "open stream");
                        let stream = self
                            .create_stream(
                                sid,
                                prio,
                                promises,
                                guaranteed_bandwidth,
                                &a2b_msg_s,
                                &a2b_close_stream_s,
                            )
                            .await;
                        b2a_stream_opened_s.send(stream).unwrap();
                        retrigger(cid, p, &mut recv_protocols);
                    },
                    Ok(ProtocolEvent::CloseStream { sid }) => {
                        trace!(?sid, "close stream");
                        self.delete_stream(sid).await;
                        retrigger(cid, p, &mut recv_protocols);
                    },
                    Ok(ProtocolEvent::Message {
                        buffer,
                        mid: _,
                        sid,
                    }) => {
                        let buffer = Arc::try_unwrap(buffer).unwrap();
                        let lock = self.streams.read().await;
                        match lock.get(&sid) {
                            Some(stream) => {
                                stream
                                    .b2a_msg_recv_s
                                    .lock()
                                    .await
                                    .send(buffer)
                                    .await
                                    .unwrap();
                            },
                            None => warn!("recv a msg with orphan stream"),
                        };
                        retrigger(cid, p, &mut recv_protocols);
                    },
                    Ok(ProtocolEvent::Shutdown) => {
                        info!(?cid, "shutdown protocol");
                        if let Err(e) = b2b_close_send_protocol_s.send(cid).await {
                            debug!(?e, ?cid, "send_mgr was already closed simultaneously");
                        }
                        if remove_c(&mut recv_protocols, &cid) {
                            break;
                        }
                    },
                    Err(e) => {
                        info!(?cid, ?e, "recv protocol failed, shutting down channel");
                        if let Err(e) = b2b_close_send_protocol_s.send(cid).await {
                            debug!(?e, ?cid, "send_mgr was already closed simultaneously");
                        }
                        if remove_c(&mut recv_protocols, &cid) {
                            break;
                        }
                    },
                }
            }
        }

        trace!("Stop recv_mgr");
        self.shutdown_barrier
            .fetch_sub(Self::BARR_RECV, Ordering::Relaxed);
    }

    async fn create_channel_mgr(
        &self,
        s2b_create_channel_r: mpsc::UnboundedReceiver<S2bCreateChannel>,
        b2b_add_send_protocol_s: mpsc::UnboundedSender<(Cid, SendProtocols)>,
        b2b_add_recv_protocol_s: mpsc::UnboundedSender<(Cid, RecvProtocols)>,
    ) {
        trace!("Start create_channel_mgr");
        let s2b_create_channel_r = UnboundedReceiverStream::new(s2b_create_channel_r);
        s2b_create_channel_r
            .for_each_concurrent(None, |(cid, _, protocol, b2s_create_channel_done_s)| {
                // This channel is now configured, and we are running it in scope of the
                // participant.
                //let w2b_frames_s = w2b_frames_s.clone();
                let channels = Arc::clone(&self.channels);
                let b2b_add_send_protocol_s = b2b_add_send_protocol_s.clone();
                let b2b_add_recv_protocol_s = b2b_add_recv_protocol_s.clone();
                async move {
                    let mut lock = channels.write().await;
                    #[cfg(feature = "metrics")]
                    let mut channel_no = lock.len();
                    lock.insert(
                        cid,
                        Mutex::new(ChannelInfo {
                            cid,
                            cid_string: cid.to_string(),
                        }),
                    );
                    drop(lock);
                    let (send, recv) = protocol.split();
                    b2b_add_send_protocol_s.send((cid, send)).unwrap();
                    b2b_add_recv_protocol_s.send((cid, recv)).unwrap();
                    b2s_create_channel_done_s.send(()).unwrap();
                    #[cfg(feature = "metrics")]
                    {
                        self.metrics
                            .channels_connected_total
                            .with_label_values(&[&self.remote_pid_string])
                            .inc();
                        if channel_no > 5 {
                            debug!(?channel_no, "metrics will overwrite channel #5");
                            channel_no = 5;
                        }
                        self.metrics
                            .participants_channel_ids
                            .with_label_values(&[&self.remote_pid_string, &channel_no.to_string()])
                            .set(cid as i64);
                    }
                }
            })
            .await;
        trace!("Stop create_channel_mgr");
        self.shutdown_barrier
            .fetch_sub(Self::BARR_CHANNEL, Ordering::Relaxed);
    }

    /// sink shutdown:
    ///  Situation AS, AR, BS, BR. A wants to close.
    ///  AS shutdown.
    ///  BR notices shutdown and tries to stops BS. (success)
    ///  BS shutdown
    ///  AR notices shutdown and tries to stop AS. (fails)
    /// For the case where BS didn't get shutdowned, e.g. by a handing situation
    /// on the remote, we have a timeout to also force close AR.
    ///
    /// This fn will:
    ///  - 1. stop api to interact with bparticipant by closing sendmsg and
    /// openstream
    ///  - 2. stop the send_mgr (it will take care of clearing the
    /// queue and finish with a Shutdown)
    ///  - (3). force stop recv after 60
    /// seconds
    ///  - (4). this fn finishes last and afterwards BParticipant
    /// drops
    ///
    /// before calling this fn, make sure `s2b_create_channel` is closed!
    /// If BParticipant kills itself managers stay active till this function is
    /// called by api to get the result status
    async fn participant_shutdown_mgr(
        &self,
        s2b_shutdown_bparticipant_r: oneshot::Receiver<S2bShutdownBparticipant>,
        b2b_close_send_protocol_s: async_channel::Sender<Cid>,
        b2b_force_close_recv_protocol_s: async_channel::Sender<Cid>,
    ) {
        let wait_for_manager = || async {
            let mut sleep = 0.01f64;
            loop {
                let bytes = self.shutdown_barrier.load(Ordering::Relaxed);
                if bytes == 0 {
                    break;
                }
                sleep *= 1.4;
                tokio::time::sleep(Duration::from_secs_f64(sleep)).await;
                if sleep > 0.2 {
                    trace!(?bytes, "wait for mgr to close");
                }
            }
        };

        trace!("Start participant_shutdown_mgr");
        let (timeout_time, sender) = s2b_shutdown_bparticipant_r.await.unwrap();
        debug!("participant_shutdown_mgr triggered");

        debug!("Closing all streams for send");
        {
            let lock = self.streams.read().await;
            for si in lock.values() {
                si.send_closed.store(true, Ordering::Relaxed);
            }
        }

        let lock = self.channels.read().await;
        assert!(
            !lock.is_empty(),
            "no channel existed remote_pid={}",
            self.remote_pid
        );
        for cid in lock.keys() {
            if let Err(e) = b2b_close_send_protocol_s.send(*cid).await {
                debug!(
                    ?e,
                    ?cid,
                    "closing send_mgr may fail if we got a recv error simultaneously"
                );
            }
        }
        drop(lock);

        trace!("wait for other managers");
        let timeout = tokio::time::sleep(timeout_time);
        let timeout = tokio::select! {
            _ = wait_for_manager() => false,
            _ = timeout => true,
        };
        if timeout {
            warn!("timeout triggered: for killing recv");
            let lock = self.channels.read().await;
            for cid in lock.keys() {
                if let Err(e) = b2b_force_close_recv_protocol_s.send(*cid).await {
                    debug!(
                        ?e,
                        ?cid,
                        "closing recv_mgr may fail if we got a recv error simultaneously"
                    );
                }
            }
        }

        trace!("wait again");
        wait_for_manager().await;

        sender.send(Ok(())).unwrap();

        #[cfg(feature = "metrics")]
        self.metrics.participants_disconnected_total.inc();
        trace!("Stop participant_shutdown_mgr");
    }

    /// Stopping API and participant usage
    /// Protocol will take care of the order of the frame
    async fn delete_stream(
        &self,
        sid: Sid,
        /* #[cfg(feature = "metrics")] frames_out_total_cache: &mut MultiCidFrameCache, */
    ) {
        let stream = { self.streams.write().await.remove(&sid) };
        match stream {
            Some(si) => {
                si.send_closed.store(true, Ordering::Relaxed);
                si.b2a_msg_recv_s.lock().await.close();
            },
            None => {
                trace!("Couldn't find the stream, might be simultaneous close from local/remote")
            },
        }
        /*
        #[cfg(feature = "metrics")]
        self.metrics
            .streams_closed_total
            .with_label_values(&[&self.remote_pid_string])
            .inc();*/
    }

    async fn create_stream(
        &self,
        sid: Sid,
        prio: Prio,
        promises: Promises,
        guaranteed_bandwidth: Bandwidth,
        a2b_msg_s: &crossbeam_channel::Sender<(Sid, Arc<MessageBuffer>)>,
        a2b_close_stream_s: &mpsc::UnboundedSender<Sid>,
    ) -> Stream {
        let (b2a_msg_recv_s, b2a_msg_recv_r) = async_channel::unbounded::<MessageBuffer>();
        let send_closed = Arc::new(AtomicBool::new(false));
        self.streams.write().await.insert(sid, StreamInfo {
            prio,
            promises,
            send_closed: Arc::clone(&send_closed),
            b2a_msg_recv_s: Mutex::new(b2a_msg_recv_s),
        });
        #[cfg(feature = "metrics")]
        self.metrics
            .streams_opened_total
            .with_label_values(&[&self.remote_pid_string])
            .inc();
        Stream::new(
            self.remote_pid,
            sid,
            prio,
            promises,
            guaranteed_bandwidth,
            send_closed,
            a2b_msg_s.clone(),
            b2a_msg_recv_r,
            a2b_close_stream_s.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        runtime::Runtime,
        sync::{mpsc, oneshot},
        task::JoinHandle,
    };

    fn mock_bparticipant() -> (
        Arc<Runtime>,
        mpsc::UnboundedSender<A2bStreamOpen>,
        mpsc::UnboundedReceiver<Stream>,
        mpsc::UnboundedSender<S2bCreateChannel>,
        oneshot::Sender<S2bShutdownBparticipant>,
        mpsc::UnboundedReceiver<B2sPrioStatistic>,
        JoinHandle<()>,
    ) {
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let runtime_clone = Arc::clone(&runtime);

        let (b2s_prio_statistic_s, b2s_prio_statistic_r) =
            mpsc::unbounded_channel::<B2sPrioStatistic>();

        let (
            bparticipant,
            a2b_open_stream_s,
            b2a_stream_opened_r,
            s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
        ) = runtime_clone.block_on(async move {
            let pid = Pid::fake(1);
            let sid = Sid::new(1000);
            let metrics = Arc::new(NetworkMetrics::new(&pid).unwrap());

            BParticipant::new(pid, sid, Arc::clone(&metrics))
        });

        let handle = runtime_clone.spawn(bparticipant.run(b2s_prio_statistic_s));
        (
            runtime_clone,
            a2b_open_stream_s,
            b2a_stream_opened_r,
            s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
            b2s_prio_statistic_r,
            handle,
        )
    }

    async fn mock_mpsc(
        cid: Cid,
        _runtime: &Arc<Runtime>,
        create_channel: &mut mpsc::UnboundedSender<S2bCreateChannel>,
    ) -> Protocols {
        let (s1, r1) = mpsc::channel(100);
        let (s2, r2) = mpsc::channel(100);
        let p1 = Protocols::new_mpsc(s1, r2);
        let (complete_s, complete_r) = oneshot::channel();
        create_channel
            .send((cid, Sid::new(0), p1, complete_s))
            .unwrap();
        complete_r.await.unwrap();
        Protocols::new_mpsc(s2, r1)
    }

    #[test]
    fn close_bparticipant_by_timeout_during_close() {
        let (
            runtime,
            a2b_open_stream_s,
            b2a_stream_opened_r,
            mut s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
            b2s_prio_statistic_r,
            handle,
        ) = mock_bparticipant();

        let _remote = runtime.block_on(mock_mpsc(0, &runtime, &mut s2b_create_channel_s));
        std::thread::sleep(Duration::from_millis(50));

        let (s, r) = oneshot::channel();
        let before = Instant::now();
        runtime.block_on(async {
            drop(s2b_create_channel_s);
            s2b_shutdown_bparticipant_s
                .send((Duration::from_secs(1), s))
                .unwrap();
            r.await.unwrap().unwrap();
        });
        assert!(
            before.elapsed() > Duration::from_millis(900),
            "timeout wasn't triggered"
        );

        runtime.block_on(handle).unwrap();

        drop((a2b_open_stream_s, b2a_stream_opened_r, b2s_prio_statistic_r));
        drop(runtime);
    }

    #[test]
    fn close_bparticipant_cleanly() {
        let (
            runtime,
            a2b_open_stream_s,
            b2a_stream_opened_r,
            mut s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
            b2s_prio_statistic_r,
            handle,
        ) = mock_bparticipant();

        let remote = runtime.block_on(mock_mpsc(0, &runtime, &mut s2b_create_channel_s));
        std::thread::sleep(Duration::from_millis(50));

        let (s, r) = oneshot::channel();
        let before = Instant::now();
        runtime.block_on(async {
            drop(s2b_create_channel_s);
            s2b_shutdown_bparticipant_s
                .send((Duration::from_secs(2), s))
                .unwrap();
            drop(remote); // remote needs to be dropped as soon as local.sender is closed
            r.await.unwrap().unwrap();
        });
        assert!(
            before.elapsed() < Duration::from_millis(1900),
            "timeout was triggered"
        );

        runtime.block_on(handle).unwrap();

        drop((a2b_open_stream_s, b2a_stream_opened_r, b2s_prio_statistic_r));
        drop(runtime);
    }

    #[test]
    fn create_stream() {
        let (
            runtime,
            a2b_open_stream_s,
            b2a_stream_opened_r,
            mut s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
            b2s_prio_statistic_r,
            handle,
        ) = mock_bparticipant();

        let remote = runtime.block_on(mock_mpsc(0, &runtime, &mut s2b_create_channel_s));
        std::thread::sleep(Duration::from_millis(50));

        // created stream
        let (rs, mut rr) = remote.split();
        let (stream_sender, _stream_receiver) = oneshot::channel();
        a2b_open_stream_s
            .send((7u8, Promises::ENCRYPTED, 1_000_000, stream_sender))
            .unwrap();

        let stream_event = runtime.block_on(rr.recv()).unwrap();
        match stream_event {
            ProtocolEvent::OpenStream {
                sid,
                prio,
                promises,
                guaranteed_bandwidth,
            } => {
                assert_eq!(sid, Sid::new(1000));
                assert_eq!(prio, 7u8);
                assert_eq!(promises, Promises::ENCRYPTED);
                assert_eq!(guaranteed_bandwidth, 1_000_000);
            },
            _ => panic!("wrong event"),
        };

        let (s, r) = oneshot::channel();
        runtime.block_on(async {
            drop(s2b_create_channel_s);
            s2b_shutdown_bparticipant_s
                .send((Duration::from_secs(1), s))
                .unwrap();
            drop((rs, rr));
            r.await.unwrap().unwrap();
        });

        runtime.block_on(handle).unwrap();

        drop((a2b_open_stream_s, b2a_stream_opened_r, b2s_prio_statistic_r));
        drop(runtime);
    }

    #[test]
    fn created_stream() {
        let (
            runtime,
            a2b_open_stream_s,
            mut b2a_stream_opened_r,
            mut s2b_create_channel_s,
            s2b_shutdown_bparticipant_s,
            b2s_prio_statistic_r,
            handle,
        ) = mock_bparticipant();

        let remote = runtime.block_on(mock_mpsc(0, &runtime, &mut s2b_create_channel_s));
        std::thread::sleep(Duration::from_millis(50));

        // create stream
        let (mut rs, rr) = remote.split();
        runtime
            .block_on(rs.send(ProtocolEvent::OpenStream {
                sid: Sid::new(1000),
                prio: 9u8,
                promises: Promises::ORDERED,
                guaranteed_bandwidth: 1_000_000,
            }))
            .unwrap();

        let stream = runtime.block_on(b2a_stream_opened_r.recv()).unwrap();
        assert_eq!(stream.promises(), Promises::ORDERED);

        let (s, r) = oneshot::channel();
        runtime.block_on(async {
            drop(s2b_create_channel_s);
            s2b_shutdown_bparticipant_s
                .send((Duration::from_secs(1), s))
                .unwrap();
            drop((rs, rr));
            r.await.unwrap().unwrap();
        });

        runtime.block_on(handle).unwrap();

        drop((a2b_open_stream_s, b2a_stream_opened_r, b2s_prio_statistic_r));
        drop(runtime);
    }
}
