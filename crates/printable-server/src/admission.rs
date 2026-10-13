//! Admission follows retained work and response streams rather than HTTP headers.

use std::sync::Arc;

use axum::{body::Body, response::Response};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub(crate) const WORK_CAPACITY: usize = 32;
const CONTROL_CAPACITY: usize = 8;
const SUBSCRIPTION_CAPACITY: usize = 16;
const BODY_CAPACITY: usize = 32;

#[derive(Clone)]
pub(crate) struct WorkPermit(Arc<OwnedSemaphorePermit>);

#[derive(Clone)]
pub(crate) struct Admission {
    cancel: CancellationToken,
    bodies: Arc<Semaphore>,
    work: Arc<Semaphore>,
    control: Arc<Semaphore>,
    subscriptions: Arc<Semaphore>,
}

impl Admission {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        let admission = Self {
            cancel: cancel.clone(),
            bodies: Arc::new(Semaphore::new(BODY_CAPACITY)),
            work: Arc::new(Semaphore::new(WORK_CAPACITY)),
            control: Arc::new(Semaphore::new(CONTROL_CAPACITY)),
            subscriptions: Arc::new(Semaphore::new(SUBSCRIPTION_CAPACITY)),
        };
        let closing = admission.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            for semaphore in [
                &closing.bodies,
                &closing.work,
                &closing.control,
                &closing.subscriptions,
            ] {
                semaphore.close();
            }
        });
        admission
    }

    pub(crate) fn body(&self) -> Option<OwnedSemaphorePermit> {
        self.bodies
            .clone()
            .try_acquire_owned()
            .ok()
            .filter(|_| !self.cancel.is_cancelled())
    }

    pub(crate) fn subscription(&self) -> Option<WorkPermit> {
        acquire(&self.subscriptions).filter(|_| !self.cancel.is_cancelled())
    }

    pub(crate) fn request(&self, request: &Value) -> Option<WorkPermit> {
        match request["method"].as_str() {
            Some("subscriptions/listen") => self.subscription(),
            Some("tools/call") if !is_control_tool(&request["params"]) => acquire(&self.work),
            _ => acquire(&self.control),
        }
        .filter(|_| !self.cancel.is_cancelled())
    }
}

fn acquire(semaphore: &Arc<Semaphore>) -> Option<WorkPermit> {
    semaphore
        .clone()
        .try_acquire_owned()
        .ok()
        .map(|permit| WorkPermit(Arc::new(permit)))
}

fn is_control_tool(params: &Value) -> bool {
    params["name"] == "status"
        || (params["name"] == "job"
            && matches!(
                params["arguments"]["action"].as_str(),
                Some("get" | "list" | "cancel")
            ))
        || (matches!(params["name"].as_str(), Some("cad_build" | "slice"))
            && matches!(
                params["arguments"]["action"].as_str(),
                Some("status" | "cancel")
            ))
        || (params["name"] == "print"
            && matches!(
                params["arguments"]["action"].as_str(),
                Some("status" | "cancel" | "pause" | "stop")
            ))
        || (params["name"] == "printer" && params["arguments"]["action"] == "status")
        || (params["name"] == "artifact" && params["arguments"]["action"] == "transfer_status")
}

/// Keep a second owner until response data finishes or the client disconnects.
/// The tool task retains its owner through effects after a disconnected stream.
pub(crate) fn retain_response(response: Response, permit: WorkPermit) -> Response {
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), permit),
        |(mut stream, permit)| async move { stream.next().await.map(|frame| (frame, (stream, permit))) },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}

impl WorkPermit {
    pub(crate) fn retained(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, action: &str) -> Value {
        json!({"method": "tools/call", "params": {"name": name, "arguments": {"action": action}}})
    }

    #[tokio::test]
    async fn saturated_work_preserves_existing_observation_and_stop_controls() {
        let admission = Admission::new(CancellationToken::new());
        let _work: Vec<_> = (0..WORK_CAPACITY)
            .map(|_| admission.request(&call("blender_execute", "")).unwrap())
            .collect();
        for (name, action) in [
            ("status", ""),
            ("job", "get"),
            ("job", "list"),
            ("job", "cancel"),
            ("cad_build", "status"),
            ("cad_build", "cancel"),
            ("slice", "status"),
            ("slice", "cancel"),
            ("print", "status"),
            ("print", "cancel"),
            ("print", "pause"),
            ("print", "stop"),
            ("printer", "status"),
            ("artifact", "transfer_status"),
        ] {
            assert!(
                admission.request(&call(name, action)).is_some(),
                "{name}.{action}"
            );
        }
        for (name, action) in [
            ("cad_build", "model"),
            ("slice", "prepare"),
            ("print", "start"),
            ("print", "clear_plate"),
            ("print", "resume"),
            ("blender_execute", ""),
            ("artifact", "upload_begin"),
            ("artifact", "upload_chunk"),
            ("artifact", "upload_commit"),
        ] {
            assert!(
                admission.request(&call(name, action)).is_none(),
                "{name}.{action}"
            );
        }
    }

    #[tokio::test]
    async fn subscriptions_are_bounded_separately_and_shutdown_closes_all_admission() {
        let cancel = CancellationToken::new();
        let admission = Admission::new(cancel.clone());
        let _listeners: Vec<_> = (0..SUBSCRIPTION_CAPACITY)
            .map(|_| admission.subscription().unwrap())
            .collect();
        assert!(admission.subscription().is_none());
        assert!(admission.request(&call("blender_execute", "")).is_some());
        assert!(admission.request(&call("status", "")).is_some());
        cancel.cancel();
        assert!(admission.body().is_none());
        assert!(admission.subscription().is_none());
        assert!(admission.request(&call("status", "")).is_none());
        assert!(admission.request(&call("blender_execute", "")).is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !admission.bodies.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(admission.body().is_none());
        assert!(admission.subscription().is_none());
        assert!(admission.request(&call("status", "")).is_none());
        assert!(admission.request(&call("blender_execute", "")).is_none());
    }
}
