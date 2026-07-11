//! Live session-signal handling for streamed turns.

use moa_core::{
    error::MoaError, error::Result, types::runtime_events::RuntimeEvent,
    types::session::SessionSignal,
};
use tokio::sync::{broadcast, mpsc};

use crate::turn::StreamSignalDisposition;

pub(super) fn handle_stream_signal(
    signal: SessionSignal,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    soft_cancel_requested: &mut bool,
) -> StreamSignalDisposition {
    match signal {
        SessionSignal::QueueMessage(_) => {
            *turn_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Message queued. Will process after current turn.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::SoftCancel => {
            *soft_cancel_requested = true;
            let _ = runtime_tx.send(RuntimeEvent::Notice(
                "Stop requested. MOA will stop after the current step.".to_string(),
            ));
            StreamSignalDisposition::Continue
        }
        SessionSignal::HardCancel => StreamSignalDisposition::CancelImmediately,
    }
}

pub(super) fn drain_signal_queue(
    signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    runtime_tx: &broadcast::Sender<RuntimeEvent>,
    turn_requested: &mut bool,
    soft_cancel_requested: &mut bool,
) -> Result<()> {
    let Some(signal_rx) = signal_rx else {
        return Ok(());
    };

    loop {
        match signal_rx.try_recv() {
            Ok(SessionSignal::QueueMessage(_)) => {
                *turn_requested = true;
                let _ = runtime_tx.send(RuntimeEvent::Notice(
                    "Message queued. Will process after current turn.".to_string(),
                ));
            }
            Ok(SessionSignal::SoftCancel) => {
                *soft_cancel_requested = true;
            }
            Ok(SessionSignal::HardCancel) => {
                *soft_cancel_requested = true;
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return Err(MoaError::ProviderError(
                    "session signal channel closed".to_string(),
                ));
            }
        }
    }
}
