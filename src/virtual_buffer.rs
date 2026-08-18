//! Workstream 8 foundation: a virtualized image buffer.
//!
//! Today images are an *observation layer* over the emulator grid: each
//! placement carries a `GraphicsGridAnchor` and is re-resolved against the
//! current scrollback/view state at render time, then the backend diffs the
//! visible submissions. This module is the first step toward making images
//! first-class citizens of a per-session **virtual buffer** that owns text rows
//! and image objects together and emits an explicit command stream as it
//! mutates — the same mutation-driven model a real graphical terminal uses for
//! its own image manager.
//!
//! This is the object model, identity registry, and command vocabulary only; it
//! is not yet wired into [`SessionGraphicsStore`](crate::graphics::SessionGraphicsStore).
//! See ROADMAP Workstream 8 for the full increment plan.

use std::collections::{BTreeMap, BTreeSet};

use crate::graphics::GraphicsResourceId;

/// A stable, per-object identity inside one session's virtual buffer. The
/// child's client `i=`/`I=` ids are mapped to this by [`ImageIdentityRegistry`].
pub type ImageObjectId = u64;

/// A decoded image resource: the pixel payload metadata plus the client's
/// `I=` image number (0 when the client used an explicit `i=` id). The encoded
/// bytes live in the graphics store and are replayed to the outer terminal on
/// upload; the buffer records only what the mutation stream needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageResource {
    /// (session, resolved client image id) — the outer-terminal resource id.
    pub resource: GraphicsResourceId,
    /// Kitty `I` image number, or 0 for an explicit `i=` creation.
    pub image_number: u32,
    pub format: u8,
    /// Bumped on every re-upload so the outer terminal re-uploads the payload.
    pub generation: u64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// The Kitty `N=1` transient usage hint.
    pub transient: bool,
}

/// One placement of an image object, attached to a virtual-buffer row.
///
/// `start_row` is a **signed, screen-relative** row: row `0` is the top of the
/// visible screen, positive rows go down the screen, and negative rows are
/// scrolled into history (|row| lines above the top). This mirrors the store's
/// anchor resolution, so history placements survive scrolls instead of being
/// deleted on underflow — eviction past the scrollback limit is the only thing
/// that drops them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImagePlacement {
    pub column: u16,
    pub start_row: i32,
    pub rows: u16,
    pub columns: u16,
    pub z_index: i32,
    pub cell_x_offset: u16,
    pub cell_y_offset: u16,
    /// The child's own placement id (`p=`), or 0 for an internally allocated
    /// key. This is the identity the child uses in `P`/`Q` relative-parent
    /// references, distinct from the outer-terminal id below.
    pub placement_id: u32,
    /// Stable outer-terminal placement id (`p=`), assigned once so the outer
    /// terminal moves the placement instead of re-creating it.
    pub outer_placement_id: u32,
    /// Relative-parent link: `(parent object id, cell_offset_x, cell_offset_y)`.
    pub parent: Option<(ImageObjectId, i32, i32)>,
    pub virtual_placement: bool,
}

impl ImagePlacement {
    /// The last row (exclusive) this placement occupies.
    pub const fn end_row(self) -> i32 {
        self.start_row + self.rows as i32
    }
}

/// An image object: a resource plus its live placements (at least one).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageObject {
    pub resource: ImageResource,
    pub placements: Vec<ImagePlacement>,
}

/// A virtual-buffer row. Text cells stay owned by the emulator grid; the
/// buffer tracks only which image objects attach here, so structural mutations
/// can move text and images together and answer "which rows does this image
/// occupy" in O(1).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VirtualRow {
    /// Object ids whose placement starts on this row.
    pub attached: BTreeSet<ImageObjectId>,
}

/// The command vocabulary the buffer emits as it mutates. Backend adapters
/// serialize these into host-terminal bytes (`a=p` moves, `d=i,...` deletes,
/// and uploads), so the outer terminal's placement state is mutation-driven
/// rather than render-diff-driven.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsCommand {
    /// Upload a resource (generation) to the outer terminal.
    Upload {
        object: ImageObjectId,
        generation: u64,
    },
    /// Place (or re-place, for a move) an object at a cell position.
    Place {
        object: ImageObjectId,
        placement: ImagePlacement,
    },
    /// Delete one placement (`placement_id`) or the whole object (`all`).
    Delete {
        object: ImageObjectId,
        placement_id: Option<u32>,
        all: bool,
    },
}

impl GraphicsCommand {
    pub const fn object(&self) -> ImageObjectId {
        match *self {
            Self::Upload { object, .. }
            | Self::Place { object, .. }
            | Self::Delete { object, .. } => object,
        }
    }

    pub const fn is_delete(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }
}

/// The identity registry: owns the child's client identity space and maps it to
/// virtual-buffer object ids, consolidating the identity handling that is
/// currently spread through the graphics store.
#[derive(Clone, Debug, Default)]
pub struct ImageIdentityRegistry {
    /// Client `i=` id → object id.
    client_to_object: BTreeMap<u32, ImageObjectId>,
    /// Client `I=` number → newest surviving object id.
    number_to_object: BTreeMap<u32, ImageObjectId>,
    /// Relative-parent identity `(client image id, placement id)` → the object
    /// that owns that placement, so `P`/`Q` references resolve to an object.
    placement_owner: BTreeMap<(u32, u32), ImageObjectId>,
}

impl ImageIdentityRegistry {
    /// Registers a client `i=` id (or the resolved internal id) for an object.
    pub fn register_client(&mut self, client_id: u32, object: ImageObjectId) {
        self.client_to_object.insert(client_id, object);
    }

    /// Registers an `I=` number; a newer registration overwrites an older one,
    /// so resolution always returns the newest surviving object with that number.
    pub fn register_number(&mut self, number: u32, object: ImageObjectId) {
        self.number_to_object.insert(number, object);
    }

    /// Registers a child placement identity `(image, p)` and the object that
    /// owns it, so a `P`/`Q` parent reference resolves to its object.
    pub fn register_placement(&mut self, image: u32, placement_id: u32, object: ImageObjectId) {
        self.placement_owner.insert((image, placement_id), object);
    }

    /// Removes every identity mapping that resolves to `object` (called when
    /// the object is deleted, so stale `I=` numbers stop resolving to it).
    pub fn forget(&mut self, object: ImageObjectId) {
        self.client_to_object.retain(|_, id| *id != object);
        self.number_to_object.retain(|_, id| *id != object);
        self.placement_owner.retain(|_, id| *id != object);
    }

    pub fn object_for_client(&self, client_id: u32) -> Option<ImageObjectId> {
        self.client_to_object.get(&client_id).copied()
    }

    pub fn object_for_number(&self, number: u32) -> Option<ImageObjectId> {
        self.number_to_object.get(&number).copied()
    }

    /// Resolves a `P`/`Q` parent reference `(image, placement_id)` to the
    /// object that owns that placement.
    pub fn object_for_parent(&self, image: u32, placement_id: u32) -> Option<ImageObjectId> {
        self.placement_owner.get(&(image, placement_id)).copied()
    }
}

/// A per-session virtual buffer: ordered rows plus the image objects attached
/// to them, and the coalesced command stream produced by buffer mutations.
#[derive(Clone, Debug, Default)]
pub struct VirtualBuffer {
    /// `rows[i]` represents screen-relative row `first_row + i`; `first_row`
    /// is the lowest tracked row (negative when history rows are present), so
    /// history rows map to the start of the vector.
    first_row: i32,
    rows: Vec<VirtualRow>,
    objects: BTreeMap<ImageObjectId, ImageObject>,
    identity: ImageIdentityRegistry,
    next_object_id: ImageObjectId,
    pending_commands: Vec<GraphicsCommand>,
}

impl VirtualBuffer {
    pub fn new() -> Self {
        Self {
            next_object_id: 1,
            ..Self::default()
        }
    }

    pub fn rows(&self) -> &[VirtualRow] {
        &self.rows
    }

    /// Returns the row at a signed screen-relative row, or `None` when that
    /// row has never had an object attached (the buffer only allocates rows it
    /// needs, so gaps are absent rather than empty).
    pub fn row_at(&self, row: i32) -> Option<&VirtualRow> {
        self.rows.get(self.row_index(row))
    }

    fn row_index(&self, row: i32) -> usize {
        (row - self.first_row) as usize
    }

    pub fn object(&self, id: ImageObjectId) -> Option<&ImageObject> {
        self.objects.get(&id)
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn pending_command_count(&self) -> usize {
        self.pending_commands.len()
    }

    pub fn identity(&self) -> &ImageIdentityRegistry {
        &self.identity
    }

    /// Registers an uploaded resource and its first placement, attaching the
    /// object to its start row and emitting an upload + place command pair.
    ///
    /// Returns the object id. If `client_id` was already registered (a
    /// re-upload of the same `i=`), the existing object's resource/generation
    /// are replaced but its placements are preserved, matching Kitty's
    /// replace-data-keep-placements behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn add_object(
        &mut self,
        client_id: u32,
        image_number: u32,
        resource: GraphicsResourceId,
        format: u8,
        generation: u64,
        pixel_width: u32,
        pixel_height: u32,
        transient: bool,
        placement: ImagePlacement,
    ) -> ImageObjectId {
        let existing = self.identity.object_for_client(client_id);
        let object = existing.unwrap_or_else(|| self.allocate_object());
        self.identity.register_client(client_id, object);
        if image_number != 0 {
            self.identity.register_number(image_number, object);
        }

        let resource = ImageResource {
            resource,
            image_number,
            format,
            generation,
            pixel_width,
            pixel_height,
            transient,
        };

        match self.objects.get_mut(&object) {
            // A re-upload of an existing `i=` replaces the image data but keeps
            // the object's placements, matching Kitty's replace-data semantics.
            Some(existing_object) => existing_object.resource = resource,
            None => {
                self.objects.insert(
                    object,
                    ImageObject {
                        resource,
                        placements: Vec::new(),
                    },
                );
                self.attach(object, placement);
            }
        }

        self.pending_commands
            .push(GraphicsCommand::Upload { object, generation });
        if existing.is_none() {
            self.identity
                .register_placement(client_id, placement.placement_id, object);
            self.pending_commands
                .push(GraphicsCommand::Place { object, placement });
        }
        object
    }

    /// Upserts a placement onto an object (by outer-terminal `p=`), emitting a
    /// place command. Re-placing the same placement (a move) updates its cell
    /// position in place rather than stacking a duplicate, while a placement
    /// with a new id is appended.
    pub fn attach_placement(&mut self, object: ImageObjectId, placement: ImagePlacement) {
        let Some(client_id) = self
            .objects
            .get(&object)
            .map(|entry| entry.resource.resource.image())
        else {
            return;
        };
        self.identity
            .register_placement(client_id, placement.placement_id, object);
        if let Some(entry) = self.objects.get_mut(&object) {
            if let Some(existing) = entry
                .placements
                .iter_mut()
                .find(|existing| existing.outer_placement_id == placement.outer_placement_id)
            {
                *existing = placement;
            } else {
                entry.placements.push(placement);
            }
        }
        self.pending_commands
            .push(GraphicsCommand::Place { object, placement });
        self.rebuild_attachments();
    }

    /// Records an upload (`a=t`, transmit-only) without attaching a placement,
    /// replacing the object's resource generation. A fresh object is created
    /// with no placements when the client id has not been seen before.
    #[allow(clippy::too_many_arguments)]
    pub fn register_upload(
        &mut self,
        client_id: u32,
        image_number: u32,
        resource: GraphicsResourceId,
        format: u8,
        generation: u64,
        pixel_width: u32,
        pixel_height: u32,
        transient: bool,
    ) -> ImageObjectId {
        let existing = self.identity.object_for_client(client_id);
        let object = existing.unwrap_or_else(|| self.allocate_object());
        self.identity.register_client(client_id, object);
        if image_number != 0 {
            self.identity.register_number(image_number, object);
        }
        let resource = ImageResource {
            resource,
            image_number,
            format,
            generation,
            pixel_width,
            pixel_height,
            transient,
        };
        match self.objects.get_mut(&object) {
            Some(existing_object) => existing_object.resource = resource,
            None => {
                self.objects.insert(
                    object,
                    ImageObject {
                        resource,
                        placements: Vec::new(),
                    },
                );
            }
        }
        self.pending_commands
            .push(GraphicsCommand::Upload { object, generation });
        object
    }

    /// Scrolls the buffer up by `rows` (a linefeed burst): every attached
    /// non-virtual object moves up, emitting one move per object. Placements
    /// that move above the top of the screen take on negative history rows but
    /// are **not** deleted — only [`Self::evict_beyond`] drops a placement
    /// once it scrolls past the configured history limit.
    pub fn scroll(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let delta = i32::try_from(rows).unwrap_or(i32::MAX);
        for (&id, object) in self.objects.iter_mut() {
            for placement in &mut object.placements {
                if placement.virtual_placement {
                    continue;
                }
                placement.start_row = placement.start_row.saturating_sub(delta);
                self.pending_commands.push(GraphicsCommand::Place {
                    object: id,
                    placement: *placement,
                });
            }
        }
        self.rebuild_attachments();
    }

    /// Deletes every placement scrolled more than `limit` history lines above
    /// the top of the screen (start row `< -limit`). Objects left with no
    /// placements are removed and their identities forgotten.
    pub fn evict_beyond(&mut self, limit: usize) {
        let limit = i32::try_from(limit).unwrap_or(i32::MAX);
        let to_evict: Vec<(ImageObjectId, u32)> = self
            .objects
            .iter()
            .flat_map(|(id, object)| {
                object.placements.iter().filter_map(move |placement| {
                    (placement.start_row < -limit).then_some((*id, placement.outer_placement_id))
                })
            })
            .collect();
        for (object, placement_id) in to_evict {
            self.delete_placement(object, placement_id);
        }
    }

    /// Deletes every object, emitting a whole-object delete for each.
    pub fn clear(&mut self) {
        let objects: Vec<ImageObjectId> = self.objects.keys().copied().collect();
        for object in objects {
            self.remove_object(object);
            self.pending_commands.push(GraphicsCommand::Delete {
                object,
                placement_id: None,
                all: true,
            });
        }
        self.rebuild_attachments();
    }

    /// Deletes an object and all its placements, dropping its identities.
    pub fn delete_object(&mut self, object: ImageObjectId) {
        self.remove_object(object);
        self.pending_commands.push(GraphicsCommand::Delete {
            object,
            placement_id: None,
            all: true,
        });
        self.rebuild_attachments();
    }

    /// Removes a single placement (by its outer-terminal `p=` id). If it was
    /// the object's last placement, the object and its identities are removed
    /// too and the delete is object-wide; otherwise the delete is scoped.
    pub fn delete_placement(&mut self, object: ImageObjectId, outer_placement_id: u32) -> bool {
        let Some(entry) = self.objects.get_mut(&object) else {
            return false;
        };
        if let Some(index) = entry
            .placements
            .iter()
            .position(|placement| placement.outer_placement_id == outer_placement_id)
        {
            entry.placements.remove(index);
        } else {
            return false;
        }
        let last = entry.placements.is_empty();
        if last {
            self.remove_object(object);
        }
        self.pending_commands.push(GraphicsCommand::Delete {
            object,
            placement_id: Some(outer_placement_id),
            all: last,
        });
        self.rebuild_attachments();
        true
    }

    /// Moves every non-virtual placement down by `rows` (inserting `rows`
    /// lines above them, e.g. a DECSTBM insert or reverse-index scroll),
    /// emitting one place per moved object.
    pub fn insert_lines(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let delta = i32::try_from(rows).unwrap_or(i32::MAX);
        for (&id, object) in self.objects.iter_mut() {
            for placement in &mut object.placements {
                if placement.virtual_placement {
                    continue;
                }
                placement.start_row = placement.start_row.saturating_add(delta);
                self.pending_commands.push(GraphicsCommand::Place {
                    object: id,
                    placement: *placement,
                });
            }
        }
        self.rebuild_attachments();
    }

    /// Drains the coalesced command stream for the current frame: at most one
    /// command per object (a delete supersedes a move/place/upload), so a burst
    /// of mutations collapses to a single, idempotent, ordered command set.
    pub fn drain_commands(&mut self) -> Vec<GraphicsCommand> {
        let commands = std::mem::take(&mut self.pending_commands);
        coalesce_commands(commands)
    }

    fn allocate_object(&mut self) -> ImageObjectId {
        let id = self.next_object_id;
        self.next_object_id = self.next_object_id.saturating_add(1).max(1);
        id
    }

    fn attach(&mut self, object: ImageObjectId, placement: ImagePlacement) {
        self.objects
            .get_mut(&object)
            .expect("object exists")
            .placements
            .push(placement);
        self.rebuild_attachments();
    }

    fn remove_object(&mut self, object: ImageObjectId) {
        self.objects.remove(&object);
        self.identity.forget(object);
    }

    /// Rebuilds the row→object attachment index from the authoritative
    /// placements. Cheap and total, so the invariant can never drift. `rows`
    /// is sized to exactly the span of occupied rows, and `row_offset` maps
    /// signed (screen-relative) rows onto the non-negative vector indices.
    fn rebuild_attachments(&mut self) {
        let mut min_row: Option<i32> = None;
        let mut max_row: Option<i32> = None;
        for object in self.objects.values() {
            for placement in &object.placements {
                min_row = Some(min_row.map_or(placement.start_row, |m| m.min(placement.start_row)));
                max_row = Some(max_row.map_or(placement.end_row(), |m| m.max(placement.end_row())));
            }
        }
        let (Some(min_row), Some(max_row)) = (min_row, max_row) else {
            self.first_row = 0;
            self.rows.clear();
            return;
        };
        self.first_row = min_row;
        let total = usize::try_from(max_row.saturating_sub(min_row)).unwrap_or(usize::MAX) + 1;
        self.rows.clear();
        self.rows.resize_with(total, VirtualRow::default);
        for (id, object) in &self.objects {
            for placement in &object.placements {
                let index = self.row_index(placement.start_row);
                self.rows[index].attached.insert(*id);
            }
        }
    }
}

/// Coalesces a burst into an idempotent, ordered command set:
/// - a whole-object `Delete` (`all: true`) supersedes every other command for
///   that object;
/// - a scoped `Delete` (`all: false`) removes only that one placement, leaving
///   places for the object's other placements intact;
/// - an `Upload` is preserved (the outer terminal must receive the payload) and
///   deduplicated per object;
/// - multiple `Place`s for the same placement (by outer `p=`) collapse to the
///   last position, while places for distinct placements are all kept.
///
/// Output order follows first appearance.
fn coalesce_commands(commands: Vec<GraphicsCommand>) -> Vec<GraphicsCommand> {
    #[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
    enum Key {
        Upload(ImageObjectId),
        Place(ImageObjectId, u32),
        WholeDelete(ImageObjectId),
        ScopedDelete(ImageObjectId, u32),
    }

    let mut order: Vec<Key> = Vec::new();
    let mut finals: BTreeMap<Key, GraphicsCommand> = BTreeMap::new();
    for command in commands {
        let object = command.object();
        let key = match &command {
            GraphicsCommand::Upload { .. } => Key::Upload(object),
            GraphicsCommand::Place { placement, .. } => {
                Key::Place(object, placement.outer_placement_id)
            }
            GraphicsCommand::Delete { all: true, .. } => Key::WholeDelete(object),
            GraphicsCommand::Delete {
                placement_id: Some(placement_id),
                ..
            } => Key::ScopedDelete(object, *placement_id),
            // `all: false` without a placement id never occurs; drop it.
            GraphicsCommand::Delete { .. } => continue,
        };
        if !finals.contains_key(&key) {
            order.push(key);
        }
        finals.insert(key, command);
    }

    let whole_deleted: BTreeSet<ImageObjectId> = order
        .iter()
        .filter_map(|key| match key {
            Key::WholeDelete(object) => Some(*object),
            _ => None,
        })
        .collect();
    let scoped_deleted: BTreeSet<(ImageObjectId, u32)> = order
        .iter()
        .filter_map(|key| match key {
            Key::ScopedDelete(object, placement_id) => Some((*object, *placement_id)),
            _ => None,
        })
        .collect();

    let mut coalesced = Vec::with_capacity(order.len());
    for key in order {
        let command = finals.remove(&key).expect("final command present");
        let object = command.object();
        match key {
            Key::WholeDelete(_) => coalesced.push(command),
            Key::Upload(_) if !whole_deleted.contains(&object) => coalesced.push(command),
            Key::Place(_, placement_id) => {
                if !whole_deleted.contains(&object)
                    && !scoped_deleted.contains(&(object, placement_id))
                {
                    coalesced.push(command);
                }
            }
            Key::ScopedDelete(_, _) => {
                if !whole_deleted.contains(&object) {
                    coalesced.push(command);
                }
            }
            Key::Upload(_) => {}
        }
    }
    coalesced
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(client_id: u32) -> GraphicsResourceId {
        GraphicsResourceId::new(crate::state::SessionId::new(1), client_id)
    }

    fn placement(start_row: i32, outer_id: u32) -> ImagePlacement {
        ImagePlacement {
            column: 0,
            start_row,
            rows: 1,
            columns: 1,
            z_index: 0,
            cell_x_offset: 0,
            cell_y_offset: 0,
            placement_id: outer_id,
            outer_placement_id: outer_id,
            parent: None,
            virtual_placement: false,
        }
    }

    fn add(buffer: &mut VirtualBuffer, client_id: u32, start_row: i32) -> ImageObjectId {
        buffer.add_object(
            client_id,
            0,
            resource(client_id),
            24,
            1,
            1,
            1,
            false,
            placement(start_row, client_id),
        )
    }

    #[test]
    fn add_object_attaches_to_its_start_row_and_emits_upload_plus_place() {
        let mut buffer = VirtualBuffer::new();
        let id = add(&mut buffer, 7, 3);

        assert_eq!(buffer.identity().object_for_client(7), Some(id));
        assert!(buffer.row_at(3).unwrap().attached.contains(&id));
        assert!(
            buffer.row_at(0).is_none(),
            "sparse rows are absent, not empty"
        );

        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 2);
        assert!(
            matches!(commands[0], GraphicsCommand::Upload { object, generation: 1 } if object == id)
        );
        assert!(
            matches!(commands[1], GraphicsCommand::Place { object, placement } if object == id && placement.start_row == 3)
        );
    }

    #[test]
    fn scroll_moves_surviving_objects_up_and_emits_one_move_each() {
        let mut buffer = VirtualBuffer::new();
        let low = add(&mut buffer, 1, 5);
        let high = add(&mut buffer, 2, 8);
        buffer.drain_commands();

        buffer.scroll(2);

        assert_eq!(buffer.object(low).unwrap().placements[0].start_row, 3);
        assert_eq!(buffer.object(high).unwrap().placements[0].start_row, 6);
        assert!(buffer.row_at(3).unwrap().attached.contains(&low));
        assert!(buffer.row_at(6).unwrap().attached.contains(&high));

        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 2);
        assert!(
            commands
                .iter()
                .all(|command| matches!(command, GraphicsCommand::Place { .. }))
        );
    }

    #[test]
    fn scroll_past_the_top_moves_the_object_into_history() {
        let mut buffer = VirtualBuffer::new();
        let id = add(&mut buffer, 1, 1);
        buffer.drain_commands();

        buffer.scroll(2);

        // The placement survives the scroll, now one row into history.
        assert_eq!(buffer.object(id).unwrap().placements[0].start_row, -1);
        assert!(buffer.row_at(-1).unwrap().attached.contains(&id));
        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(commands[0], GraphicsCommand::Place { object, placement } if object == id && placement.start_row == -1)
        );
    }

    #[test]
    fn evict_beyond_deletes_placements_scrolled_past_the_limit() {
        let mut buffer = VirtualBuffer::new();
        let kept = add(&mut buffer, 1, 3);
        let evicted = add(&mut buffer, 2, 1);
        buffer.drain_commands();

        // Scroll both up by 4: `kept` lands at -1 (retained), `evicted` at -3.
        buffer.scroll(4);
        buffer.evict_beyond(2);

        assert_eq!(buffer.object(kept).unwrap().placements[0].start_row, -1);
        assert!(buffer.object(evicted).is_none());
        let commands = buffer.drain_commands();
        assert_eq!(
            commands.len(),
            2,
            "the retained move and the eviction delete"
        );
        assert!(commands.iter().any(
            |command| matches!(command, GraphicsCommand::Place { object, placement } if *object == kept && placement.start_row == -1)
        ));
        assert!(commands.iter().any(
            |command| matches!(command, GraphicsCommand::Delete { object, all: true, .. } if *object == evicted)
        ));
    }

    #[test]
    fn identity_numbers_resolve_to_the_newest_surviving_object() {
        let mut buffer = VirtualBuffer::new();
        let first = buffer.add_object(1, 5, resource(1), 24, 1, 1, 1, false, placement(0, 1));
        let second = buffer.add_object(2, 5, resource(2), 24, 1, 1, 1, false, placement(1, 2));

        assert_eq!(buffer.identity().object_for_number(5), Some(second));
        assert_ne!(first, second);

        buffer.delete_object(second);
        assert_eq!(buffer.identity().object_for_number(5), None);
    }

    #[test]
    fn re_upload_reuses_the_object_and_replaces_the_resource() {
        let mut buffer = VirtualBuffer::new();
        let id = add(&mut buffer, 7, 2);
        buffer.drain_commands();

        let again = buffer.add_object(7, 0, resource(7), 24, 2, 2, 2, false, placement(2, 7));

        assert_eq!(
            id, again,
            "re-uploading the same client id reuses the object"
        );
        let object = buffer.object(id).unwrap();
        assert_eq!(object.resource.generation, 2);
        assert_eq!(
            object.placements.len(),
            1,
            "a re-upload replaces data, not placements"
        );
    }

    #[test]
    fn a_burst_of_scrolls_coalesces_to_one_move_per_object() {
        let mut buffer = VirtualBuffer::new();
        let id = add(&mut buffer, 1, 20);
        buffer.drain_commands();

        for _ in 0..20 {
            buffer.scroll(1);
        }

        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1, "20 scrolls collapse to one move");
        assert!(
            matches!(commands[0], GraphicsCommand::Place { object, placement } if object == id && placement.start_row == 0)
        );
    }

    #[test]
    fn coalescing_keeps_a_scoped_delete_beside_a_place_for_a_different_placement() {
        let commands = vec![
            GraphicsCommand::Upload {
                object: 1,
                generation: 2,
            },
            GraphicsCommand::Delete {
                object: 1,
                placement_id: Some(1),
                all: false,
            },
            GraphicsCommand::Place {
                object: 1,
                placement: placement(3, 2),
            },
        ];
        let coalesced = coalesce_commands(commands);
        assert_eq!(
            coalesced.len(),
            3,
            "a re-transmit keeps upload, scoped delete, and the new place"
        );
        assert!(matches!(coalesced[0], GraphicsCommand::Upload { .. }));
        assert!(matches!(
            &coalesced[1],
            GraphicsCommand::Delete {
                placement_id: Some(1),
                all: false,
                ..
            }
        ));
        assert!(matches!(
            &coalesced[2],
            GraphicsCommand::Place { placement, .. } if placement.outer_placement_id == 2
        ));
    }

    #[test]
    fn coalescing_whole_object_delete_supersedes_upload_and_places() {
        let commands = vec![
            GraphicsCommand::Upload {
                object: 1,
                generation: 1,
            },
            GraphicsCommand::Place {
                object: 1,
                placement: placement(0, 1),
            },
            GraphicsCommand::Delete {
                object: 1,
                placement_id: None,
                all: true,
            },
        ];
        let coalesced = coalesce_commands(commands);
        assert_eq!(coalesced.len(), 1);
        assert!(matches!(
            &coalesced[0],
            GraphicsCommand::Delete { all: true, .. }
        ));
    }

    #[test]
    fn parent_references_resolve_to_the_owning_object() {
        let mut buffer = VirtualBuffer::new();
        let parent = buffer.add_object(1, 0, resource(1), 24, 1, 1, 1, false, placement(2, 1));
        buffer.drain_commands();

        assert_eq!(
            buffer.identity().object_for_parent(1, 1),
            Some(parent),
            "the parent placement identity resolves to its owning object"
        );
    }

    #[test]
    fn attach_placement_adds_a_second_placement_and_registers_its_identity() {
        let mut buffer = VirtualBuffer::new();
        let object = add(&mut buffer, 7, 2);
        buffer.drain_commands();

        buffer.attach_placement(object, placement(4, 99));

        assert_eq!(buffer.object(object).unwrap().placements.len(), 2);
        assert_eq!(buffer.identity().object_for_parent(7, 99), Some(object));
        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(commands[0], GraphicsCommand::Place { object: id, placement } if id == object && placement.outer_placement_id == 99)
        );
    }

    #[test]
    fn delete_placement_scopes_to_one_placement_then_removes_the_object() {
        let mut buffer = VirtualBuffer::new();
        let object = add(&mut buffer, 7, 2);
        buffer.attach_placement(object, placement(4, 99));
        buffer.drain_commands();

        assert!(buffer.delete_placement(object, 99));
        assert_eq!(buffer.object(object).unwrap().placements.len(), 1);
        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(&commands[0], GraphicsCommand::Delete { object: id, placement_id: Some(99), all: false } if *id == object)
        );

        assert!(buffer.delete_placement(object, 7));
        assert!(buffer.object(object).is_none());
        let commands = buffer.drain_commands();
        assert!(
            matches!(&commands[0], GraphicsCommand::Delete { object: id, all: true, .. } if *id == object)
        );
    }

    #[test]
    fn insert_lines_moves_placements_down_but_not_virtual_ones() {
        let mut buffer = VirtualBuffer::new();
        let real = add(&mut buffer, 1, 2);
        let virtual_ = buffer.add_object(
            2,
            0,
            resource(2),
            24,
            1,
            1,
            1,
            false,
            ImagePlacement {
                column: 0,
                start_row: 3,
                rows: 1,
                columns: 1,
                z_index: 0,
                cell_x_offset: 0,
                cell_y_offset: 0,
                placement_id: 2,
                outer_placement_id: 2,
                parent: None,
                virtual_placement: true,
            },
        );
        buffer.drain_commands();

        buffer.insert_lines(2);

        assert_eq!(buffer.object(real).unwrap().placements[0].start_row, 4);
        assert_eq!(
            buffer.object(virtual_).unwrap().placements[0].start_row,
            3,
            "virtual placements never scroll"
        );
        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1, "only the real placement moves");
        assert!(
            matches!(commands[0], GraphicsCommand::Place { object: id, placement } if id == real && placement.start_row == 4)
        );
    }

    #[test]
    fn register_upload_creates_a_placementless_object() {
        let mut buffer = VirtualBuffer::new();
        let object = buffer.register_upload(9, 0, resource(9), 32, 3, 8, 4, false);

        let entry = buffer.object(object).unwrap();
        assert!(entry.placements.is_empty());
        assert_eq!(entry.resource.generation, 3);
        let commands = buffer.drain_commands();
        assert_eq!(commands.len(), 1);
        assert!(
            matches!(commands[0], GraphicsCommand::Upload { object: id, generation: 3 } if id == object)
        );
    }
}
