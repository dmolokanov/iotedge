use std::{future::Future, pin::Pin};

use futures_util::FutureExt;
use tokio::sync::mpsc::{self, Receiver, Sender};

#[async_trait::async_trait]
pub trait Sidecar {
    fn shutdown_handle(&self) -> Result<SidecarShutdownHandle, SidecarShutdownHandleError>;

    async fn run(self: Box<Self>);
}

pub struct SidecarShutdownHandle(Pin<Box<dyn Future<Output = ()>>>);

impl SidecarShutdownHandle {
    pub fn new<F>(shutdown: F) -> Self
    where
        F: Future<Output = ()> + 'static,
    {
        Self(Box::pin(shutdown))
    }

    pub async fn shutdown(self) {
        self.0.await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unable to obtain shutdown handler for sidecar")]
pub struct SidecarShutdownHandleError;

pub fn pending() -> PendingSidecar {
    let (rx, tx) = mpsc::channel(1);
    PendingSidecar(rx, tx)
}

pub struct PendingSidecar(Sender<()>, Receiver<()>);

#[async_trait::async_trait]
impl Sidecar for PendingSidecar {
    fn shutdown_handle(&self) -> Result<SidecarShutdownHandle, SidecarShutdownHandleError> {
        let mut handle = self.0.clone();
        let shutdown = async move {
            handle.send(()).map(drop).await;
        };
        Ok(SidecarShutdownHandle::new(shutdown))
    }

    async fn run(mut self: Box<Self>) {
        self.1.recv().await;
    }
}
