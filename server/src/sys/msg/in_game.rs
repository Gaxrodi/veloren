use super::super::SysTimer;
use crate::{client::Client, metrics::NetworkRequestMetrics, presence::Presence, Settings};
use common::{
    comp::{CanBuild, ControlEvent, Controller, ForceUpdate, Health, Ori, Pos, Stats, Vel},
    event::{EventBus, ServerEvent},
    msg::{ClientGeneral, PresenceKind, ServerGeneral},
    span,
    terrain::{TerrainChunkSize, TerrainGrid},
    vol::{ReadVol, RectVolSize},
};
use common_sys::state::BlockChange;
use specs::{Entities, Join, Read, ReadExpect, ReadStorage, System, Write, WriteStorage};
use tracing::{debug, trace};

impl Sys {
    #[allow(clippy::too_many_arguments)]
    fn handle_client_in_game_msg(
        server_emitter: &mut common::event::Emitter<'_, ServerEvent>,
        entity: specs::Entity,
        client: &Client,
        maybe_presence: &mut Option<&mut Presence>,
        terrain: &ReadExpect<'_, TerrainGrid>,
        network_metrics: &ReadExpect<'_, NetworkRequestMetrics>,
        can_build: &ReadStorage<'_, CanBuild>,
        force_updates: &ReadStorage<'_, ForceUpdate>,
        stats: &mut WriteStorage<'_, Stats>,
        healths: &ReadStorage<'_, Health>,
        block_changes: &mut Write<'_, BlockChange>,
        positions: &mut WriteStorage<'_, Pos>,
        velocities: &mut WriteStorage<'_, Vel>,
        orientations: &mut WriteStorage<'_, Ori>,
        controllers: &mut WriteStorage<'_, Controller>,
        settings: &Read<'_, Settings>,
        msg: ClientGeneral,
    ) -> Result<(), crate::error::Error> {
        let presence = match maybe_presence {
            Some(g) => g,
            None => {
                debug!(?entity, "client is not in_game, ignoring msg");
                trace!(?msg, "ignored msg content");
                if matches!(msg, ClientGeneral::TerrainChunkRequest{ .. }) {
                    network_metrics.chunks_request_dropped.inc();
                }
                return Ok(());
            },
        };
        match msg {
            // Go back to registered state (char selection screen)
            ClientGeneral::ExitInGame => {
                server_emitter.emit(ServerEvent::ExitIngame { entity });
                client.send(ServerGeneral::ExitInGameSuccess)?;
                *maybe_presence = None;
            },
            ClientGeneral::SetViewDistance(view_distance) => {
                presence.view_distance = settings
                    .max_view_distance
                    .map(|max| view_distance.min(max))
                    .unwrap_or(view_distance);

                //correct client if its VD is to high
                if settings
                    .max_view_distance
                    .map(|max| view_distance > max)
                    .unwrap_or(false)
                {
                    client.send(ServerGeneral::SetViewDistance(
                        settings.max_view_distance.unwrap_or(0),
                    ))?;
                }
            },
            ClientGeneral::ControllerInputs(inputs) => {
                if matches!(presence.kind, PresenceKind::Character(_)) {
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.inputs.update_with_new(inputs);
                    }
                }
            },
            ClientGeneral::ControlEvent(event) => {
                if matches!(presence.kind, PresenceKind::Character(_)) {
                    // Skip respawn if client entity is alive
                    if let ControlEvent::Respawn = event {
                        if healths.get(entity).map_or(true, |h| !h.is_dead) {
                            //Todo: comment why return!
                            return Ok(());
                        }
                    }
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.events.push(event);
                    }
                }
            },
            ClientGeneral::ControlAction(event) => {
                if matches!(presence.kind, PresenceKind::Character(_)) {
                    if let Some(controller) = controllers.get_mut(entity) {
                        controller.actions.push(event);
                    }
                }
            },
            ClientGeneral::PlayerPhysics { pos, vel, ori } => {
                if matches!(presence.kind, PresenceKind::Character(_))
                    && force_updates.get(entity).is_none()
                    && healths.get(entity).map_or(true, |h| !h.is_dead)
                {
                    let _ = positions.insert(entity, pos);
                    let _ = velocities.insert(entity, vel);
                    let _ = orientations.insert(entity, ori);
                }
            },
            ClientGeneral::BreakBlock(pos) => {
                if let Some(block) = can_build.get(entity).and_then(|_| terrain.get(pos).ok()) {
                    block_changes.set(pos, block.into_vacant());
                }
            },
            ClientGeneral::PlaceBlock(pos, block) => {
                if can_build.get(entity).is_some() {
                    block_changes.try_set(pos, block);
                }
            },
            ClientGeneral::TerrainChunkRequest { key } => {
                let in_vd = if let Some(pos) = positions.get(entity) {
                    pos.0.xy().map(|e| e as f64).distance(
                        key.map(|e| e as f64 + 0.5) * TerrainChunkSize::RECT_SIZE.map(|e| e as f64),
                    ) < (presence.view_distance as f64 - 1.0 + 2.5 * 2.0_f64.sqrt())
                        * TerrainChunkSize::RECT_SIZE.x as f64
                } else {
                    true
                };
                if in_vd {
                    match terrain.get_key(key) {
                        Some(chunk) => {
                            network_metrics.chunks_served_from_memory.inc();
                            client.send(ServerGeneral::TerrainChunkUpdate {
                                key,
                                chunk: Ok(Box::new(chunk.clone())),
                            })?
                        },
                        None => {
                            network_metrics.chunks_generation_triggered.inc();
                            server_emitter.emit(ServerEvent::ChunkRequest(entity, key))
                        },
                    }
                } else {
                    network_metrics.chunks_request_dropped.inc();
                }
            },
            ClientGeneral::UnlockSkill(skill) => {
                stats
                    .get_mut(entity)
                    .map(|s| s.skill_set.unlock_skill(skill));
            },
            ClientGeneral::RefundSkill(skill) => {
                stats
                    .get_mut(entity)
                    .map(|s| s.skill_set.refund_skill(skill));
            },
            ClientGeneral::UnlockSkillGroup(skill_group_type) => {
                stats
                    .get_mut(entity)
                    .map(|s| s.skill_set.unlock_skill_group(skill_group_type));
            },
            _ => unreachable!("not a client_in_game msg"),
        }
        Ok(())
    }
}

/// This system will handle new messages from clients
pub struct Sys;
impl<'a> System<'a> for Sys {
    #[allow(clippy::type_complexity)]
    type SystemData = (
        Entities<'a>,
        Read<'a, EventBus<ServerEvent>>,
        ReadExpect<'a, TerrainGrid>,
        ReadExpect<'a, NetworkRequestMetrics>,
        Write<'a, SysTimer<Self>>,
        ReadStorage<'a, CanBuild>,
        ReadStorage<'a, ForceUpdate>,
        WriteStorage<'a, Stats>,
        ReadStorage<'a, Health>,
        Write<'a, BlockChange>,
        WriteStorage<'a, Pos>,
        WriteStorage<'a, Vel>,
        WriteStorage<'a, Ori>,
        WriteStorage<'a, Presence>,
        WriteStorage<'a, Client>,
        WriteStorage<'a, Controller>,
        Read<'a, Settings>,
    );

    fn run(
        &mut self,
        (
            entities,
            server_event_bus,
            terrain,
            network_metrics,
            mut timer,
            can_build,
            force_updates,
            mut stats,
            healths,
            mut block_changes,
            mut positions,
            mut velocities,
            mut orientations,
            mut presences,
            mut clients,
            mut controllers,
            settings,
        ): Self::SystemData,
    ) {
        span!(_guard, "run", "msg::in_game::Sys::run");
        timer.start();

        let mut server_emitter = server_event_bus.emitter();

        for (entity, client, mut maybe_presence) in
            (&entities, &mut clients, (&mut presences).maybe()).join()
        {
            let _ = super::try_recv_all(client, 2, |client, msg| {
                Self::handle_client_in_game_msg(
                    &mut server_emitter,
                    entity,
                    client,
                    &mut maybe_presence,
                    &terrain,
                    &network_metrics,
                    &can_build,
                    &force_updates,
                    &mut stats,
                    &healths,
                    &mut block_changes,
                    &mut positions,
                    &mut velocities,
                    &mut orientations,
                    &mut controllers,
                    &settings,
                    msg,
                )
            });
        }

        timer.end()
    }
}
