use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    ProgressTick,
    QueueChanged,
    CatalogChanged,
    UpdatesChanged,
}

pub type EventBus = broadcast::Sender<Event>;

pub fn new_bus() -> EventBus {
    broadcast::channel(256).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscriber_receives_emitted_event() {
        let bus = new_bus();
        let mut rx = bus.subscribe();
        bus.send(Event::QueueChanged).unwrap();
        assert_eq!(rx.recv().await.unwrap(), Event::QueueChanged);
    }

    #[tokio::test]
    async fn send_with_no_subscribers_is_not_an_error_for_callers() {
        // broadcast::send returns Err when there are no receivers; we ignore it.
        let bus = new_bus();
        let _ = bus.send(Event::ProgressTick); // must not panic
    }
}
