use afs_connector::{Connector, FetchRequest};
use afs_core::AfsResult;
use afs_notion::NotionConnector;

use crate::hydration::{HydratedEntity, HydrationSource};

impl HydrationSource for NotionConnector {
    fn fetch_render(
        &self,
        request: &afs_core::hydration::HydrationRequest,
    ) -> AfsResult<HydratedEntity> {
        let native = self.fetch(FetchRequest {
            remote_id: request.remote_id.clone(),
        })?;
        let rendered = self.render_native_entity(&native)?;

        Ok(HydratedEntity {
            document: rendered.document,
            shadow: rendered.shadow,
        })
    }
}
