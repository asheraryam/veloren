/// event_mapper::movement watches all local entities movements and determines
/// which sfx to emit, and the position at which the sound should be emitted
/// from
use crate::audio::sfx::{SfxTriggerItem, SfxTriggers};

use common::{
    comp::{
        item::{Item, ItemKind},
        Body, CharacterState, ItemConfig, Loadout, PhysicsState, Pos, Vel,
    },
    event::{EventBus, SfxEvent, SfxEventItem},
    state::State,
};
use hashbrown::HashMap;
use specs::{Entity as EcsEntity, Join, WorldExt};
use std::time::{Duration, Instant};
use vek::*;

#[derive(Clone)]
struct PreviousEntityState {
    event: SfxEvent,
    time: Instant,
    weapon_drawn: bool,
    on_ground: bool,
}

impl Default for PreviousEntityState {
    fn default() -> Self {
        Self {
            event: SfxEvent::Idle,
            time: Instant::now(),
            weapon_drawn: false,
            on_ground: true,
        }
    }
}

pub struct MovementEventMapper {
    event_history: HashMap<EcsEntity, PreviousEntityState>,
}

impl MovementEventMapper {
    pub fn new() -> Self {
        Self {
            event_history: HashMap::new(),
        }
    }

    pub fn maintain(&mut self, state: &State, player_entity: EcsEntity, triggers: &SfxTriggers) {
        const SFX_DIST_LIMIT_SQR: f32 = 20000.0;
        let ecs = state.ecs();

        let sfx_event_bus = ecs.read_resource::<EventBus<SfxEventItem>>();
        let mut sfx_emitter = sfx_event_bus.emitter();

        let player_position = ecs
            .read_storage::<Pos>()
            .get(player_entity)
            .map_or(Vec3::zero(), |pos| pos.0);

        for (entity, pos, vel, body, physics, loadout, character) in (
            &ecs.entities(),
            &ecs.read_storage::<Pos>(),
            &ecs.read_storage::<Vel>(),
            &ecs.read_storage::<Body>(),
            &ecs.read_storage::<PhysicsState>(),
            ecs.read_storage::<Loadout>().maybe(),
            ecs.read_storage::<CharacterState>().maybe(),
        )
            .join()
            .filter(|(_, e_pos, ..)| {
                (e_pos.0.distance_squared(player_position)) < SFX_DIST_LIMIT_SQR
            })
        {
            if let Some(character) = character {
                let state = self
                    .event_history
                    .entry(entity)
                    .or_insert_with(|| PreviousEntityState::default());

                let mapped_event = match body {
                    Body::Humanoid(_) => {
                        Self::map_movement_event(character, physics, state, vel.0, loadout)
                    },
                    Body::QuadrupedMedium(_)
                    | Body::QuadrupedSmall(_)
                    | Body::BirdMedium(_)
                    | Body::BirdSmall(_)
                    | Body::BipedLarge(_) => Self::map_non_humanoid_movement_event(physics, vel.0),
                    _ => SfxEvent::Idle, // Ignore fish, critters, etc...
                };

                // Check for SFX config entry for this movement
                if Self::should_emit(state, triggers.get_key_value(&mapped_event)) {
                    sfx_emitter.emit(SfxEventItem::new(
                        mapped_event,
                        Some(pos.0),
                        Some(Self::get_volume_for_body_type(body)),
                    ));

                    state.time = Instant::now();
                }

                // update state to determine the next event. We only record the time (above) if
                // it was dispatched
                state.event = mapped_event;
                state.weapon_drawn = Self::weapon_drawn(character);
                state.on_ground = physics.on_ground;
            }
        }

        self.cleanup(player_entity);
    }

    /// As the player explores the world, we track the last event of the nearby
    /// entities to determine the correct SFX item to play next based on
    /// their activity. `cleanup` will remove entities from event tracking if
    /// they have not triggered an event for > n seconds. This prevents
    /// stale records from bloating the Map size.
    fn cleanup(&mut self, player: EcsEntity) {
        const TRACKING_TIMEOUT: u64 = 10;

        let now = Instant::now();
        self.event_history.retain(|entity, event| {
            now.duration_since(event.time) < Duration::from_secs(TRACKING_TIMEOUT)
                || entity.id() == player.id()
        });
    }

    /// When specific entity movements are detected, the associated sound (if
    /// any) needs to satisfy two conditions to be allowed to play:
    /// 1. An sfx.ron entry exists for the movement (we need to know which sound
    /// file(s) to play) 2. The sfx has not been played since it's timeout
    /// threshold has elapsed, which prevents firing every tick
    fn should_emit(
        previous_state: &PreviousEntityState,
        sfx_trigger_item: Option<(&SfxEvent, &SfxTriggerItem)>,
    ) -> bool {
        if let Some((event, item)) = sfx_trigger_item {
            if &previous_state.event == event {
                previous_state.time.elapsed().as_secs_f64() >= item.threshold
            } else {
                true
            }
        } else {
            false
        }
    }

    /// Voxygen has an existing list of character states however that list does
    /// not provide enough resolution to target specific entity events, such
    /// as opening or closing the glider. These methods translate those
    /// entity states with some additional data into more specific
    /// `SfxEvent`'s which we attach sounds to
    fn map_movement_event(
        character_state: &CharacterState,
        physics_state: &PhysicsState,
        previous_state: &PreviousEntityState,
        vel: Vec3<f32>,
        loadout: Option<&Loadout>,
    ) -> SfxEvent {
        // Handle wield state changes
        if let Some(active_loadout) = loadout {
            if let Some(ItemConfig {
                item:
                    Item {
                        kind: ItemKind::Tool(data),
                        ..
                    },
                ..
            }) = active_loadout.active_item
            {
                if let Some(wield_event) = match (
                    previous_state.weapon_drawn,
                    character_state.is_dodge(),
                    Self::weapon_drawn(character_state),
                ) {
                    (false, false, true) => Some(SfxEvent::Wield(data.kind)),
                    (true, false, false) => Some(SfxEvent::Unwield(data.kind)),
                    _ => None,
                } {
                    return wield_event;
                }
            }
        }

        // Match run state
        if physics_state.on_ground && vel.magnitude() > 0.1
            || !previous_state.on_ground && physics_state.on_ground
        {
            return SfxEvent::Run;
        }

        // Match all other Movemement and Action states
        match (previous_state.event, character_state) {
            (_, CharacterState::Roll { .. }) => SfxEvent::Roll,
            (_, CharacterState::Climb { .. }) => SfxEvent::Climb,
            (SfxEvent::Glide, CharacterState::Idle { .. }) => SfxEvent::GliderClose,
            (previous_event, CharacterState::Glide { .. }) => {
                if previous_event != SfxEvent::GliderOpen && previous_event != SfxEvent::Glide {
                    SfxEvent::GliderOpen
                } else {
                    SfxEvent::Glide
                }
            },
            _ => SfxEvent::Idle,
        }
    }

    /// Maps a limited set of movements for other non-humanoid entities
    fn map_non_humanoid_movement_event(physics_state: &PhysicsState, vel: Vec3<f32>) -> SfxEvent {
        if physics_state.on_ground && vel.magnitude() > 0.1 {
            SfxEvent::Run
        } else {
            SfxEvent::Idle
        }
    }

    /// This helps us determine whether we should be emitting the Wield/Unwield
    /// events. For now, consider either CharacterState::Wielding or
    /// ::Equipping to mean the weapon is drawn. This will need updating if the
    /// animations change to match the wield_duration associated with the weapon
    fn weapon_drawn(character: &CharacterState) -> bool {
        character.is_wield()
            || match character {
                CharacterState::Equipping { .. } => true,
                _ => false,
            }
    }

    /// Returns a relative volume value for a body type. This helps us emit sfx
    /// at a volume appropriate fot the entity we are emitting the event for
    fn get_volume_for_body_type(body: &Body) -> f32 {
        match body {
            Body::Humanoid(_) => 0.9,
            Body::QuadrupedSmall(_) => 0.3,
            Body::QuadrupedMedium(_) => 0.7,
            Body::BirdMedium(_) => 0.3,
            Body::BirdSmall(_) => 0.2,
            Body::BipedLarge(_) => 1.0,
            _ => 0.9,
        }
    }
}

#[cfg(test)] mod tests;
