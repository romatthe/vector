use crate::Event;
use async_trait::async_trait;
use futures::Stream;

#[async_trait]
pub trait StreamingSink: Send {
    async fn run(
        &mut self,
        input: Box<dyn Stream<Item = Result<Event, ()>> + Send>,
    ) -> Result<(), ()>;
}
