//! Korean WashTower dryer half (`BDH_D39301_KR`, deviceType 222, tag `0x34`).
//! Shares protocol codec and control logic with [`crate::devices::fakpk21021`].

use crate::device_trait::DeviceHandler;
use rethink_core::ha::HaConnection;
use rethink_core::metadata::Metadata;
use rethink_core::thinq::Thinq2Device;
use std::sync::Arc;

pub fn create(
    ha: Arc<dyn HaConnection>,
    thinq: Arc<dyn Thinq2Device>,
    meta: Metadata,
) -> Arc<dyn DeviceHandler> {
    crate::devices::fakpk21021::create_dryer(ha, thinq, meta)
}
