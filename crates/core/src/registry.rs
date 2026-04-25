use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use crate::bus::EventBus;
use crate::event::Event;
use crate::model::{Device, DeviceGroup, DeviceId, GroupId, Room, RoomId};
use crate::validation::validate_device;

#[derive(Debug, Clone)]
pub struct DeviceRegistry {
    bus: EventBus,
    devices: Arc<RwLock<HashMap<DeviceId, Device>>>,
    rooms: Arc<RwLock<HashMap<RoomId, Room>>>,
    groups: Arc<RwLock<HashMap<GroupId, DeviceGroup>>>,
}

impl DeviceRegistry {
    pub fn new(bus: EventBus) -> Self {
        Self {
            bus,
            devices: Arc::new(RwLock::new(HashMap::new())),
            rooms: Arc::new(RwLock::new(HashMap::new())),
            groups: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn upsert(&self, device: Device) -> Result<()> {
        validate_device(&device)?;
        validate_room_assignment(self, &device)?;

        let event = {
            let mut devices = write_guard(&self.devices);
            let id = device.id.clone();
            match devices.get_mut(&id) {
                None => {
                    devices.insert(id, device.clone());
                    Some(Event::DeviceAdded { device })
                }
                Some(existing) => {
                    let state_changed = device.kind != existing.kind
                        || device.attributes != existing.attributes
                        || device.metadata != existing.metadata;

                    if state_changed {
                        let previous_attributes = existing.attributes.clone();
                        *existing = device.clone();
                        Some(Event::DeviceStateChanged {
                            id,
                            attributes: device.attributes.clone(),
                            previous_attributes,
                        })
                    } else if device.last_seen != existing.last_seen {
                        existing.last_seen = device.last_seen;
                        Some(Event::DeviceSeen {
                            id,
                            last_seen: existing.last_seen,
                        })
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(event) = event {
            self.bus.publish(event);
        }
        Ok(())
    }

    pub fn restore(&self, devices: Vec<Device>) -> Result<()> {
        let mut restored = HashMap::with_capacity(devices.len());

        for device in devices {
            validate_device(&device)?;
            validate_room_assignment(self, &device)?;
            restored.insert(device.id.clone(), device);
        }

        let mut current = write_guard(&self.devices);
        *current = restored;

        Ok(())
    }

    pub fn restore_rooms(&self, rooms: Vec<Room>) {
        let mut restored = HashMap::with_capacity(rooms.len());

        for room in rooms {
            restored.insert(room.id.clone(), room);
        }

        let mut current = write_guard(&self.rooms);
        *current = restored;
    }

    pub fn restore_groups(&self, groups: Vec<DeviceGroup>) {
        let devices = read_guard(&self.devices);
        let mut restored = HashMap::with_capacity(groups.len());

        for mut group in groups {
            let before = group.members.len();
            group.members.retain(|id| devices.contains_key(id));
            let pruned = before - group.members.len();
            if pruned > 0 {
                tracing::warn!(
                    group_id = %group.id.0,
                    pruned,
                    "pruned {pruned} unknown member(s) from group '{}' during restore; \
                     the device(s) were likely removed while the server was offline",
                    group.id.0
                );
            }
            restored.insert(group.id.clone(), group);
        }

        let mut current = write_guard(&self.groups);
        *current = restored;
    }

    pub async fn remove(&self, id: &DeviceId) -> bool {
        let removed = {
            let mut devices = write_guard(&self.devices);
            devices.remove(id).is_some()
        };

        if !removed {
            return false;
        }

        self.bus.publish(Event::DeviceRemoved { id: id.clone() });

        let changed_groups = {
            let mut groups = write_guard(&self.groups);
            let mut changed = Vec::new();

            for group in groups.values_mut() {
                let previous_len = group.members.len();
                group.members.retain(|member_id| member_id != id);
                if group.members.len() != previous_len {
                    changed.push((group.id.clone(), group.members.clone()));
                }
            }

            changed
        };

        for (group_id, members) in changed_groups {
            self.bus.publish(Event::GroupMembersChanged {
                id: group_id,
                members,
            });
        }

        true
    }

    pub fn get(&self, id: &DeviceId) -> Option<Device> {
        let devices = read_guard(&self.devices);
        devices.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Device> {
        let devices = read_guard(&self.devices);
        devices.values().cloned().collect()
    }

    /// Returns all devices whose ID is prefixed with `"{adapter}:"`.
    ///
    /// This is the efficient way to fetch all entities from a single adapter
    /// (e.g. every `open_meteo:*` device for a weather card) without a
    /// client-side filter pass over the full device list.
    pub fn list_devices_for_adapter(&self, adapter: &str) -> Vec<Device> {
        let prefix = format!("{}:", adapter);
        let devices = read_guard(&self.devices);
        devices
            .values()
            .filter(|d| d.id.0.starts_with(&prefix))
            .cloned()
            .collect()
    }

    pub async fn upsert_room(&self, room: Room) {
        let event = {
            let mut rooms = write_guard(&self.rooms);
            match rooms.insert(room.id.clone(), room.clone()) {
                Some(_) => Event::RoomUpdated { room },
                None => Event::RoomAdded { room },
            }
        };

        self.bus.publish(event);
    }

    pub async fn remove_room(&self, id: &RoomId) -> bool {
        let removed = {
            let mut rooms = write_guard(&self.rooms);
            rooms.remove(id).is_some()
        };

        if !removed {
            return false;
        }

        let affected_devices = {
            let mut devices = write_guard(&self.devices);
            let mut changed = Vec::new();

            for device in devices.values_mut() {
                if device.room_id.as_ref() == Some(id) {
                    device.room_id = None;
                    changed.push(device.id.clone());
                }
            }

            changed
        };

        self.bus.publish(Event::RoomRemoved { id: id.clone() });

        for device_id in affected_devices {
            self.bus.publish(Event::DeviceRoomChanged {
                id: device_id,
                room_id: None,
            });
        }

        true
    }

    pub fn get_room(&self, id: &RoomId) -> Option<Room> {
        let rooms = read_guard(&self.rooms);
        rooms.get(id).cloned()
    }

    pub fn list_rooms(&self) -> Vec<Room> {
        let rooms = read_guard(&self.rooms);
        rooms.values().cloned().collect()
    }

    pub fn list_devices_in_room(&self, room_id: &RoomId) -> Vec<Device> {
        let devices = read_guard(&self.devices);
        devices
            .values()
            .filter(|device| device.room_id.as_ref() == Some(room_id))
            .cloned()
            .collect()
    }

    pub async fn upsert_group(&self, group: DeviceGroup) -> Result<()> {
        validate_group(&group)?;

        {
            let devices = read_guard(&self.devices);
            validate_group_membership(&devices, &group)?;
        }

        let event = {
            let mut groups = write_guard(&self.groups);
            match groups.insert(group.id.clone(), group.clone()) {
                Some(_) => Event::GroupUpdated { group },
                None => Event::GroupAdded { group },
            }
        };

        self.bus.publish(event);
        Ok(())
    }

    pub async fn remove_group(&self, id: &GroupId) -> bool {
        let removed = {
            let mut groups = write_guard(&self.groups);
            groups.remove(id).is_some()
        };

        if removed {
            self.bus.publish(Event::GroupRemoved { id: id.clone() });
        }

        removed
    }

    pub fn get_group(&self, id: &GroupId) -> Option<DeviceGroup> {
        let groups = read_guard(&self.groups);
        groups.get(id).cloned()
    }

    pub fn list_groups(&self) -> Vec<DeviceGroup> {
        let groups = read_guard(&self.groups);
        groups.values().cloned().collect()
    }

    pub fn list_devices_in_group(&self, group_id: &GroupId) -> Vec<Device> {
        let members = {
            let groups = read_guard(&self.groups);
            groups
                .get(group_id)
                .map(|group| group.members.clone())
                .unwrap_or_default()
        };

        let devices = read_guard(&self.devices);
        members
            .into_iter()
            .filter_map(|id| devices.get(&id).cloned())
            .collect()
    }

    pub async fn set_group_members(
        &self,
        group_id: &GroupId,
        members: Vec<DeviceId>,
    ) -> Result<bool> {
        let deduped_members = dedupe_member_ids(members);

        {
            let devices = read_guard(&self.devices);
            for member in &deduped_members {
                if !devices.contains_key(member) {
                    anyhow::bail!(
                        "group '{}' references unknown device '{}'",
                        group_id.0,
                        member.0
                    );
                }
            }
        }

        let changed = {
            let mut groups = write_guard(&self.groups);
            let Some(group) = groups.get_mut(group_id) else {
                return Ok(false);
            };

            if group.members == deduped_members {
                return Ok(true);
            }

            group.members = deduped_members.clone();
            true
        };

        if changed {
            self.bus.publish(Event::GroupMembersChanged {
                id: group_id.clone(),
                members: deduped_members,
            });
        }

        Ok(true)
    }

    pub async fn assign_device_to_room(
        &self,
        device_id: &DeviceId,
        room_id: Option<RoomId>,
    ) -> Result<bool> {
        if let Some(room_id) = &room_id {
            let rooms = read_guard(&self.rooms);
            if !rooms.contains_key(room_id) {
                anyhow::bail!("room '{}' not found", room_id.0);
            }
        }

        let changed = {
            let mut devices = write_guard(&self.devices);
            let Some(device) = devices.get_mut(device_id) else {
                return Ok(false);
            };

            if device.room_id == room_id {
                return Ok(true);
            }

            device.room_id = room_id.clone();
            true
        };

        if changed {
            self.bus.publish(Event::DeviceRoomChanged {
                id: device_id.clone(),
                room_id,
            });
        }

        Ok(true)
    }
}

fn validate_room_assignment(registry: &DeviceRegistry, device: &Device) -> Result<()> {
    let Some(room_id) = &device.room_id else {
        return Ok(());
    };

    let rooms = read_guard(&registry.rooms);
    if rooms.contains_key(room_id) {
        Ok(())
    } else {
        anyhow::bail!(
            "device '{}' references unknown room '{}'",
            device.id.0,
            room_id.0
        )
    }
}

fn validate_group(group: &DeviceGroup) -> Result<()> {
    if group.id.0.trim().is_empty() {
        anyhow::bail!("group id must not be empty");
    }
    if group.name.trim().is_empty() {
        anyhow::bail!("group name must not be empty");
    }

    Ok(())
}

fn validate_group_membership(
    devices: &HashMap<DeviceId, Device>,
    group: &DeviceGroup,
) -> Result<()> {
    for member in &group.members {
        if !devices.contains_key(member) {
            anyhow::bail!(
                "group '{}' references unknown device '{}'",
                group.id.0,
                member.0
            );
        }
    }

    Ok(())
}

fn dedupe_member_ids(members: Vec<DeviceId>) -> Vec<DeviceId> {
    let mut seen = std::collections::HashSet::new();
    members
        .into_iter()
        .filter(|member| seen.insert(member.clone()))
        .collect()
}

fn read_guard<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_guard<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
