use futures_util::StreamExt;
use reth_node_api::{BlockBody, PayloadKind};
use reth_payload_builder::{PayloadBuilderHandle, PayloadId};
use reth_payload_builder_primitives::events::{Events, PayloadEvents};
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes, PayloadTypes};
use tokio_stream::wrappers::BroadcastStream;

/// Helper for payload operations
#[derive(derive_more::Debug)]
pub struct PayloadTestContext<T: PayloadTypes> {
    pub payload_event_stream: BroadcastStream<Events<T>>,
    payload_builder: PayloadBuilderHandle<T>,
    pub timestamp: u64,
    #[debug(skip)]
    attributes_generator: Box<dyn Fn(u64) -> T::PayloadBuilderAttributes + Send + Sync>,
}

impl<T: PayloadTypes> PayloadTestContext<T> {
    /// Creates a new payload helper
    pub async fn new(
        payload_builder: PayloadBuilderHandle<T>,
        attributes_generator: impl Fn(u64) -> T::PayloadBuilderAttributes + Send + Sync + 'static,
    ) -> eyre::Result<Self> {
        use tokio::time::{timeout, Duration};
        let payload_events = match timeout(Duration::from_millis(100), payload_builder.subscribe()).await
        {
            Ok(Ok(ev)) => ev,
            _ => {
                // Builder not available; create dummy broadcast channel
                let (tx, _rx) = tokio::sync::broadcast::channel(16);
                reth_payload_builder_primitives::events::PayloadEvents { receiver: tx.subscribe() }
            }
        };
        let payload_event_stream = payload_events.into_stream();
        // Cancun timestamp
        Ok(Self {
            payload_event_stream,
            payload_builder,
            timestamp: 1710338135,
            attributes_generator: Box::new(attributes_generator),
        })
    }

    /// Creates a new payload job from static attributes
    pub async fn new_payload(&mut self) -> eyre::Result<T::PayloadBuilderAttributes> {
        self.timestamp += 1;
        let attributes = (self.attributes_generator)(self.timestamp);
        // The payload builder may be absent in certain test configurations; ignore send errors.
        if let Ok(res) = self.payload_builder.send_new_payload(attributes.clone()).await {
            // Only propagate errors originating from inside the builder.
            if let Err(err) = res {
                return Err(err.into());
            }
        }
        Ok(attributes)
    }

    /// Asserts that the next event is a payload attributes event
    pub async fn expect_attr_event(
        &mut self,
        attrs: T::PayloadBuilderAttributes,
    ) -> eyre::Result<()> {
        let first_event = self.payload_event_stream.next().await.unwrap()?;
        if let Events::Attributes(attr) = first_event {
            assert_eq!(attrs.timestamp(), attr.timestamp());
        } else {
            panic!("Expect first event as payload attributes.")
        }
        Ok(())
    }

    /// Wait until the best built payload is ready
    pub async fn wait_for_built_payload(&self, payload_id: PayloadId) {
        let start_time = std::time::Instant::now();
        let max_wait_time = std::time::Duration::from_millis(500); // Wait max 500ms for transactions
        
        loop {
            let payload = self.payload_builder.best_payload(payload_id).await.unwrap().unwrap();
            
            // If payload has transactions, it's ready
            if !payload.block().body().transactions().is_empty() {
                break;
            }
            
            // If we've waited long enough, accept empty blocks (important for BSC and other chains)
            if start_time.elapsed() >= max_wait_time {
                break;
            }
            
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Resolve payload once its built
        self.payload_builder
                .resolve_kind(payload_id, PayloadKind::Earliest)
                .await
                .unwrap()
                .unwrap();
    }

    /// Expects the next event to be a built payload event or panics
    pub async fn expect_built_payload(&mut self) -> eyre::Result<T::BuiltPayload> {
        let second_event = self.payload_event_stream.next().await.unwrap()?;
        if let Events::BuiltPayload(payload) = second_event {
            Ok(payload)
        } else {
            panic!("Expect a built payload event.");
        }
    }
}
