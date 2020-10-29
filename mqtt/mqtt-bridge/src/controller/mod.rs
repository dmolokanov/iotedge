mod bridges;

use bridges::Bridges;

use async_trait::async_trait;
use futures_util::{
    future::{self, Either},
    stream::Fuse,
    FusedStream, StreamExt,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info, warn};

use mqtt_broker::sidecar::{Sidecar, SidecarShutdownHandle, SidecarShutdownHandleError};

use crate::{
    bridge::{Bridge, BridgeError},
    config_update::BridgeControllerUpdate,
    settings::BridgeSettings,
};

const UPSTREAM: &str = "$upstream";

/// `BridgeController` controls lifetime of bridges: start/stop and update
/// forwarding rules.
///
/// Controller handles monitors settings updates and starts a new `Bridge` or
/// stops running `Bridge` if the number of bridges changes. In addition it
/// prepares changes in forwarding rules and applies them to `Bridge` if require.
pub struct BridgeController {
    system_address: String,
    device_id: String,
    settings: BridgeSettings,
    handle: BridgeControllerHandle,
    messages: Fuse<UnboundedReceiver<BridgeControllerMessage>>,
}

impl BridgeController {
    pub fn new(system_address: String, device_id: String, settings: BridgeSettings) -> Self {
        let (sender, updates_receiver) = mpsc::unbounded_channel();
        let handle = BridgeControllerHandle { sender };

        Self {
            system_address,
            device_id,
            settings,
            handle,
            messages: updates_receiver.fuse(),
        }
    }

    pub fn handle(&self) -> BridgeControllerHandle {
        self.handle.clone()
    }
}

#[async_trait]
impl Sidecar for BridgeController {
    fn shutdown_handle(&self) -> Result<SidecarShutdownHandle, SidecarShutdownHandleError> {
        let handle = self.handle.clone();
        Ok(SidecarShutdownHandle::new(async { handle.shutdown() }))
    }

    async fn run(mut self: Box<Self>) {
        info!("starting bridge controller...");

        let mut bridges = Bridges::default();

        if let Some(upstream_settings) = self.settings.upstream() {
            match Bridge::new_upstream(&self.system_address, &self.device_id, upstream_settings) {
                Ok(bridge) => {
                    bridges.start_bridge(bridge, upstream_settings).await;
                }
                Err(e) => {
                    error!(err = %e, "failed to create {} bridge", UPSTREAM);
                }
            }
        } else {
            info!("no upstream settings detected.")
        }

        let mut no_bridges = bridges.is_terminated();

        loop {
            let wait_bridge_or_pending = if no_bridges {
                // if no active bridges available, wait only for a new messages arrival
                Either::Left(future::pending())
            } else {
                // otherwise try to await both a new message arrival or any bridge exit
                Either::Right(bridges.next())
            };

            match future::select(self.messages.select_next_some(), wait_bridge_or_pending).await {
                Either::Left((BridgeControllerMessage::BridgeControllerUpdate(update), _)) => {
                    process_update(update, &mut bridges).await
                }
                Either::Left((BridgeControllerMessage::Shutdown, _)) => {
                    info!("bridge controller shutdown requested");
                    bridges.shutdown_all().await;
                    break;
                }
                Either::Right((Some((name, bridge)), _)) => {
                    match bridge {
                        Ok(Ok(_)) => debug!("bridge {} exited", name),
                        Ok(Err(e)) => warn!(error = %e, "bridge {} exited with error", name),
                        Err(e) => warn!(error = %e, "bridge {} panicked ", name),
                    }

                    info!("restarting bridge...");
                    if let Some(upstream_settings) = self.settings.upstream() {
                        match Bridge::new_upstream(
                            &self.system_address,
                            &self.device_id,
                            upstream_settings,
                        ) {
                            Ok(bridge) => {
                                bridges.start_bridge(bridge, upstream_settings).await;
                            }
                            Err(e) => {
                                error!(err = %e, "failed to create {} bridge", name);
                            }
                        }
                    }
                }
                Either::Right((None, _)) => {
                    // first time we resolve bridge future it returns None
                    no_bridges = true;
                }
            }
        }

        info!("finished bridge controller");
    }
}

async fn process_update(update: BridgeControllerUpdate, bridges: &mut Bridges) {
    debug!("received updated config: {:?}", update);

    for bridge_update in update.into_inner() {
        // for now only supports upstream bridge.
        if bridge_update.name() != UPSTREAM {
            warn!(
                "updates for {} bridge is not supported",
                bridge_update.name()
            );
            continue;
        }

        bridges.send_update(bridge_update).await;
    }
}

#[derive(Clone, Debug)]
pub struct BridgeControllerHandle {
    sender: UnboundedSender<BridgeControllerMessage>,
}

impl BridgeControllerHandle {
    pub fn send_update(&mut self, update: BridgeControllerUpdate) -> Result<(), Error> {
        self.send_message(BridgeControllerMessage::BridgeControllerUpdate(update))
    }

    pub fn shutdown(mut self) {
        if let Err(e) = self.send_message(BridgeControllerMessage::Shutdown) {
            error!(error = %e, "unable to request shutdown for bridge controller");
        }
    }

    fn send_message(&mut self, message: BridgeControllerMessage) -> Result<(), Error> {
        self.sender
            .send(message)
            .map_err(Error::SendControllerMessage)
    }
}

/// Control message for `BridgeController`.
#[derive(Debug)]
pub enum BridgeControllerMessage {
    BridgeControllerUpdate(BridgeControllerUpdate),
    Shutdown,
}

/// Error for `BridgeController`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("An error occurred sending a message to the controller.")]
    SendControllerMessage(#[source] tokio::sync::mpsc::error::SendError<BridgeControllerMessage>),

    #[error("An error occurred sending a message to the bridge.")]
    SendBridgeMessage(#[from] BridgeError),
}
