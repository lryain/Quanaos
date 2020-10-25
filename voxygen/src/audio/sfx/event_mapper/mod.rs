mod block;
mod combat;
mod movement;
mod progression;

use common::{state::State, terrain::TerrainChunk};

use block::BlockEventMapper;
use combat::CombatEventMapper;
use movement::MovementEventMapper;
use progression::ProgressionEventMapper;

use super::SfxTriggers;
use crate::scene::{Camera, Terrain};

trait EventMapper {
    fn maintain(
        &mut self,
        state: &State,
        player_entity: specs::Entity,
        camera: &Camera,
        triggers: &SfxTriggers,
        terrain: &Terrain<TerrainChunk>,
    );
}

pub struct SfxEventMapper {
    mappers: Vec<Box<dyn EventMapper>>,
}

impl SfxEventMapper {
    pub fn new() -> Self {
        Self {
            mappers: vec![
                Box::new(CombatEventMapper::new()),
                Box::new(MovementEventMapper::new()),
                Box::new(ProgressionEventMapper::new()),
                Box::new(BlockEventMapper::new()),
            ],
        }
    }

    pub fn maintain(
        &mut self,
        state: &State,
        player_entity: specs::Entity,
        camera: &Camera,
        triggers: &SfxTriggers,
        terrain: &Terrain<TerrainChunk>,
    ) {
        for mapper in &mut self.mappers {
            mapper.maintain(state, player_entity, camera, triggers, terrain);
        }
    }
}
