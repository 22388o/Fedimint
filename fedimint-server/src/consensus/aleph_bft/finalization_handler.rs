use fedimint_core::PeerId;

use super::data_provider::UnitData;

pub struct FinalizationHandler {
    sender: async_channel::Sender<(UnitData, PeerId)>,
}

impl FinalizationHandler {
    pub fn new(sender: async_channel::Sender<(UnitData, PeerId)>) -> Self {
        Self { sender }
    }
}

impl aleph_bft::FinalizationHandler<UnitData> for FinalizationHandler {
    fn data_finalized(&mut self, unit_data: UnitData, creator: aleph_bft::NodeIndex) {
        // the channel is unbounded
        self.sender
            .try_send((unit_data, super::to_peer_id(creator)))
            .ok();
    }
}
