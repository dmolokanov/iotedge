use std::future::Future;

use anyhow::Result;
use futures_util::{
    future::{self, Either},
    pin_mut,
};
use mqtt_broker::{BrokerHandle, BrokerSnapshot, Message, Sidecar, SystemEvent};

use tracing::{debug, error};

#[cfg(feature = "edgehub")]
mod edgehub;

#[cfg(feature = "edgehub")]
pub use edgehub::{add_sidecars, broker, config, start_server};

#[cfg(all(not(feature = "edgehub"), feature = "generic"))]
mod generic;

#[cfg(all(not(feature = "edgehub"), feature = "generic"))]
pub use generic::{broker, config, start_server, start_sidecars, SidecarError};

pub struct SidecarManager {
    sidecars: Vec<Box<dyn Sidecar>>,
}

impl SidecarManager {
    pub fn new() -> Self {
        Self {
            sidecars: Vec::new(),
        }
    }

    pub fn add_sidecar<S: Sidecar + 'static>(&mut self, sidecar: S) {
        self.sidecars.push(Box::new(sidecar));
    }

    pub async fn run(
        self,
        mut broker_handle: BrokerHandle,
        server: impl Future<Output = Result<BrokerSnapshot>>,
    ) -> Result<BrokerSnapshot> {
        let (mut shutdowns, sidecars): (Vec<_>, Vec<_>) = self
            .sidecars
            .into_iter()
            .map(|sidecar| (sidecar.shutdown_handle(), tokio::spawn(sidecar.run())))
            .unzip();

        pin_mut!(server);

        let state = match future::select(server, future::select_all(sidecars)).await {
            // server exited first
            Either::Left((snapshot, sidecars)) => {
                // send shutdown event to each sidecar
                let shutdowns = shutdowns.into_iter().map(|handle| handle.shutdown());
                future::join_all(shutdowns).await;

                // awaits for at least one to finish
                let (_res, stopped, mut sidecars) = sidecars.await;
                sidecars.remove(stopped);

                // wait for the rest to exit
                future::join_all(sidecars).await;

                snapshot?
            }
            // one of sidecars exited first
            Either::Right(((res, stopped, sidecars), server)) => {
                // signal server
                broker_handle.send(Message::System(SystemEvent::Shutdown))?;
                let snapshot = server.await;

                shutdowns.remove(stopped);

                debug!("a sidecar has stopped. shutting down all sidecars...");
                if let Err(e) = res {
                    error!(message = "failed waiting for sidecar shutdown", error = %e);
                }

                // send shutdown event to each sidecar
                let shutdowns = shutdowns.into_iter().map(|handle| handle.shutdown());
                future::join_all(shutdowns).await;

                // wait for the rest to exit
                future::join_all(sidecars).await;

                snapshot?
            }
        };

        Ok(state)
    }
}
