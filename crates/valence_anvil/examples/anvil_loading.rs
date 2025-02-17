use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;

use clap::Parser;
use flume::{Receiver, Sender};
use tracing::warn;
use valence::bevy_app::AppExit;
use valence::client::despawn_disconnected_clients;
use valence::client::event::default_event_handler;
use valence::prelude::*;
use valence_anvil::{AnvilChunk, AnvilWorld};

const SPAWN_POS: DVec3 = DVec3::new(0.0, 256.0, 0.0);
const SECTION_COUNT: usize = 24;

#[derive(Parser)]
#[clap(author, version, about)]
struct Cli {
    /// The path to a Minecraft world save containing a `region` subdirectory.
    path: PathBuf,
}

#[derive(Resource)]
struct GameState {
    /// Chunks that need to be generated. Chunks without a priority have already
    /// been sent to the anvil thread.
    pending: HashMap<ChunkPos, Option<Priority>>,
    sender: Sender<ChunkPos>,
    receiver: Receiver<(ChunkPos, Chunk)>,
}

/// The order in which chunks should be processed by anvil worker. Smaller
/// values are sent first.
type Priority = u64;

pub fn main() {
    tracing_subscriber::fmt().init();

    App::new()
        .add_plugin(ServerPlugin::new(()))
        .add_system_to_stage(EventLoop, default_event_handler)
        .add_system_set(PlayerList::default_system_set())
        .add_startup_system(setup)
        .add_system(init_clients)
        .add_system(remove_unviewed_chunks.after(init_clients))
        .add_system(update_client_views.after(remove_unviewed_chunks))
        .add_system(send_recv_chunks.after(update_client_views))
        .add_system(despawn_disconnected_clients)
        .run();
}

fn setup(world: &mut World) {
    let cli = Cli::parse();
    let dir = cli.path;

    if !dir.exists() {
        eprintln!("Directory `{}` does not exist. Exiting.", dir.display());
        world.send_event(AppExit);
    } else if !dir.is_dir() {
        eprintln!("`{}` is not a directory. Exiting.", dir.display());
        world.send_event(AppExit);
    }

    let anvil = AnvilWorld::new(dir);

    let (finished_sender, finished_receiver) = flume::unbounded();
    let (pending_sender, pending_receiver) = flume::unbounded();

    // Process anvil chunks in a different thread to avoid blocking the main tick
    // loop.
    thread::spawn(move || anvil_worker(pending_receiver, finished_sender, anvil));

    world.insert_resource(GameState {
        pending: HashMap::new(),
        sender: pending_sender,
        receiver: finished_receiver,
    });

    let instance = world
        .resource::<Server>()
        .new_instance(DimensionId::default());

    world.spawn(instance);
}

fn init_clients(
    mut clients: Query<&mut Client, Added<Client>>,
    instances: Query<Entity, With<Instance>>,
    mut commands: Commands,
) {
    for mut client in &mut clients {
        let instance = instances.single();

        client.set_flat(true);
        client.set_game_mode(GameMode::Creative);
        client.set_position(SPAWN_POS);
        client.set_instance(instance);

        commands.spawn(McEntity::with_uuid(
            EntityKind::Player,
            instance,
            client.uuid(),
        ));
    }
}

fn remove_unviewed_chunks(mut instances: Query<&mut Instance>) {
    instances
        .single_mut()
        .retain_chunks(|_, chunk| chunk.is_viewed_mut());
}

fn update_client_views(
    mut instances: Query<&mut Instance>,
    mut clients: Query<&mut Client>,
    mut state: ResMut<GameState>,
) {
    let instance = instances.single_mut();

    for client in &mut clients {
        let view = client.view();
        let queue_pos = |pos| {
            if instance.chunk(pos).is_none() {
                match state.pending.entry(pos) {
                    Entry::Occupied(mut oe) => {
                        if let Some(priority) = oe.get_mut() {
                            let dist = view.pos.distance_squared(pos);
                            *priority = (*priority).min(dist);
                        }
                    }
                    Entry::Vacant(ve) => {
                        let dist = view.pos.distance_squared(pos);
                        ve.insert(Some(dist));
                    }
                }
            }
        };

        // Queue all the new chunks in the view to be sent to the anvil worker.
        if client.is_added() {
            view.iter().for_each(queue_pos);
        } else {
            let old_view = client.old_view();
            if old_view != view {
                view.diff(old_view).for_each(queue_pos);
            }
        }
    }
}

fn send_recv_chunks(mut instances: Query<&mut Instance>, state: ResMut<GameState>) {
    let mut instance = instances.single_mut();
    let state = state.into_inner();

    // Insert the chunks that are finished loading into the instance.
    for (pos, chunk) in state.receiver.drain() {
        instance.insert_chunk(pos, chunk);
        assert!(state.pending.remove(&pos).is_some());
    }

    // Collect all the new chunks that need to be loaded this tick.
    let mut to_send = vec![];

    for (pos, priority) in &mut state.pending {
        if let Some(pri) = priority.take() {
            to_send.push((pri, pos));
        }
    }

    // Sort chunks by ascending priority.
    to_send.sort_unstable_by_key(|(pri, _)| *pri);

    // Send the sorted chunks to be loaded.
    for (_, pos) in to_send {
        let _ = state.sender.try_send(*pos);
    }
}

fn anvil_worker(
    receiver: Receiver<ChunkPos>,
    sender: Sender<(ChunkPos, Chunk)>,
    mut world: AnvilWorld,
) {
    while let Ok(pos) = receiver.recv() {
        match get_chunk(pos, &mut world) {
            Ok(chunk) => {
                if let Some(chunk) = chunk {
                    let _ = sender.try_send((pos, chunk));
                }
            }
            Err(e) => warn!("Failed to get chunk at ({}, {}): {e:#}.", pos.x, pos.z),
        }
    }
}

fn get_chunk(pos: ChunkPos, world: &mut AnvilWorld) -> anyhow::Result<Option<Chunk>> {
    let Some(AnvilChunk { data, .. }) = world.read_chunk(pos.x, pos.z)? else {
        return Ok(None)
    };

    let mut chunk = Chunk::new(SECTION_COUNT);

    valence_anvil::to_valence(&data, &mut chunk, 4, |_| BiomeId::default())?;

    Ok(Some(chunk))
}
