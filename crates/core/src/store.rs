use crate::model::{Device, DeviceId, Room, RoomId};

#[async_trait::async_trait]
pub trait DeviceStore: Send + Sync + 'static {
    async fn load_all_devices(&self) -> anyhow::Result<Vec<Device>>;
    async fn load_all_rooms(&self) -> anyhow::Result<Vec<Room>>;
    async fn save_device(&self, device: &Device) -> anyhow::Result<()>;
    async fn save_room(&self, room: &Room) -> anyhow::Result<()>;
    async fn delete_device(&self, id: &DeviceId) -> anyhow::Result<()>;
    async fn delete_room(&self, id: &RoomId) -> anyhow::Result<()>;
}
