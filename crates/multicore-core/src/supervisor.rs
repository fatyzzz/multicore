use std::{future::Future, pin::Pin, sync::Arc};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Xray,
    Mihomo,
}

pub trait ProcessController: Send + Sync {
    type Error: Send;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn diagnostics<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = crate::sidecar::ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async { crate::sidecar::ProcessDiagnostics::default() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SupervisorError {
    #[error("failed to start {engine:?}")]
    StartFailed {
        engine: Engine,
        rollback_failed: bool,
    },
    #[error("failed to stop one or more sidecars")]
    StopFailed,
}

pub struct Supervisor<P: ProcessController + ?Sized> {
    controller: Arc<P>,
    operation: tokio::sync::Mutex<()>,
}

impl<P: ProcessController + ?Sized> Supervisor<P> {
    pub fn new(controller: Arc<P>) -> Self {
        Self {
            controller,
            operation: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn connect(&self) -> Result<(), SupervisorError> {
        let _operation = self.operation.lock().await;
        self.controller
            .start(Engine::Xray)
            .await
            .map_err(|_| SupervisorError::StartFailed {
                engine: Engine::Xray,
                rollback_failed: false,
            })?;

        if self.controller.start(Engine::Mihomo).await.is_err() {
            let rollback_failed = self.controller.stop(Engine::Xray).await.is_err();
            return Err(SupervisorError::StartFailed {
                engine: Engine::Mihomo,
                rollback_failed,
            });
        }
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<(), SupervisorError> {
        let _operation = self.operation.lock().await;
        let mihomo = self.controller.stop(Engine::Mihomo).await;
        let xray = self.controller.stop(Engine::Xray).await;
        if mihomo.is_err() || xray.is_err() {
            Err(SupervisorError::StopFailed)
        } else {
            Ok(())
        }
    }
}
