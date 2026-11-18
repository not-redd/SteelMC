//! End-to-end check that a converted world looks the same to a real client.
//!
//! The vanilla fixture is converted, the real `steel` binary serves it, and an Azalea bot
//! decodes every chunk packet it receives. Each block of every chunk vanilla saved as `full`
//! is compared against the original `.mca` data, read with an independent reader.
//!
//! Vanilla itself changes some chunks before it sends them (see `chunks_vanilla_may_change`).
//! Differences there are counted and printed but do not fail the test; `steel_load` holds
//! those chunks to their saved blocks instead.
//!
//! Run with:
//!
//! ```text
//! cargo test -p steel-converter-prototype --test azalea_e2e -- --ignored --nocapture
//! ```
//!
//! Set `STEEL_BIN` to an already-built `steel` binary to skip `cargo build -p steel`.

mod support;

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    time::Duration,
};

use azalea::core::position::ChunkSectionBlockPos;
use azalea::protocol::packets::game::ClientboundGamePacket;
use azalea::registry::builtin::BlockKind;
use azalea::{
    Client, ClientInformation, Event,
    account::Account,
    block::{BlockState, BlockTrait},
    world::Chunk,
};
use rustc_hash::FxHashMap;
use steel_converter_prototype::{ConvertOptions, convert};
use support::{
    steel_server::{ServerSettings, SteelServer},
    vanilla_world::{VanillaChunk, VanillaRegions, canonical},
};
use tokio::{task::LocalSet, time::timeout};

/// Vanilla's maximum client view distance. The fixture's full chunks span x -22..17 and
/// z -18..23 around a spawn at chunk (0, 0), so this reaches all of them without moving.
const VIEW_DISTANCE: u8 = 32;
const SIMULATION_DISTANCE: u8 = 4;
const OVERWORLD_MIN_Y: i32 = -64;
const OVERWORLD_HEIGHT: u32 = 384;
const RECEIVE_TIMEOUT: Duration = Duration::from_mins(15);
const MAX_REPORTED_MISMATCHES: usize = 40;

fn fixture_world() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test_worlds/vanilla_26_3")
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear previous e2e dir");
    }
    fs::create_dir_all(&dir).expect("create e2e dir");
    dir
}

/// Random ticks, mob griefing and fire would change blocks between Steel loading a chunk and
/// sending it. They are gameplay, not conversion, so the test turns them off in the converted
/// `level.toml` before Steel starts.
fn freeze_block_changing_gameplay(level_toml: &Path) {
    let mut level: toml::Table = fs::read_to_string(level_toml)
        .expect("read converted level.toml")
        .parse()
        .expect("parse converted level.toml");
    let rules = level
        .get_mut("game_rules")
        .and_then(toml::Value::as_table_mut)
        .expect("converted level.toml has game_rules");
    rules.insert("random_tick_speed".into(), 0.into());
    rules.insert("mob_griefing".into(), false.into());
    rules.insert("fire_spread_radius_around_player".into(), 0.into());
    fs::write(level_toml, level.to_string()).expect("write level.toml");
}

/// The chunk holding the world spawn in a converted `level.toml`.
fn spawn_chunk(level_toml: &Path) -> (i32, i32) {
    let level: toml::Table = fs::read_to_string(level_toml)
        .expect("read converted level.toml")
        .parse()
        .expect("parse converted level.toml");
    let coordinate = |axis: &str| {
        let block = level
            .get("spawn")
            .and_then(|spawn| spawn.get(axis))
            .and_then(toml::Value::as_integer)
            .expect("converted level.toml has an integer spawn");
        i32::try_from(block.div_euclid(16)).expect("spawn chunk fits in i32")
    };
    (coordinate("x"), coordinate("z"))
}

/// Full chunks that vanilla may change between loading and sending, so they cannot be held to
/// their saved blocks.
///
/// Vanilla sends a chunk only once it is ticking, right after `postProcessGeneration` applies its
/// saved `PostProcessing` updates. Saved block and fluid ticks run within the block-ticking
/// range, simulation distance + 1. The effects of both can spill into the next chunk, and the
/// bot may spawn up to `respawn_radius` (10 blocks), one more chunk, from the world spawn.
fn chunks_vanilla_may_change(
    vanilla: &VanillaRegions,
    full: &BTreeSet<(i32, i32)>,
    spawn: (i32, i32),
) -> BTreeSet<(i32, i32)> {
    let tick_reach = i32::from(SIMULATION_DISTANCE) + 3;
    let mut queued_work: FxHashMap<(i32, i32), (bool, bool)> = FxHashMap::default();
    let mut queued = |pos: (i32, i32)| {
        *queued_work.entry(pos).or_insert_with(|| {
            vanilla
                .read_chunk_nbt(pos.0, pos.1)
                .map_or((false, false), |chunk| {
                    let post_processing = chunk
                        .list("PostProcessing")
                        .and_then(|sections| sections.lists())
                        .is_some_and(|sections| {
                            sections
                                .iter()
                                .any(|section| section.shorts().is_some_and(|s| !s.is_empty()))
                        });
                    let ticks = ["block_ticks", "fluid_ticks"].iter().any(|name| {
                        chunk
                            .list(name)
                            .and_then(|ticks| ticks.compounds())
                            .is_some_and(|ticks| !ticks.is_empty())
                    });
                    (post_processing, ticks)
                })
        })
    };
    full.iter()
        .copied()
        .filter(|&(x, z)| {
            let mut post_processing = false;
            let mut ticks = false;
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let (p, t) = queued((x + dx, z + dz));
                    post_processing |= p;
                    ticks |= t;
                }
            }
            let tick_distance = (x - spawn.0).abs().max((z - spawn.1).abs());
            post_processing || (ticks && tick_distance <= tick_reach)
        })
        .collect()
}

#[derive(Default)]
struct Comparison {
    received: BTreeSet<(i32, i32)>,
    /// From `chunks_vanilla_may_change`.
    may_change: BTreeSet<(i32, i32)>,
    mismatched_blocks: usize,
    mismatched_chunks: BTreeSet<(i32, i32)>,
    report: String,
    /// Differences in `may_change` chunks, which do not fail the test.
    tolerated_blocks: usize,
    tolerated_chunks: BTreeSet<(i32, i32)>,
    state_names: FxHashMap<BlockState, String>,
}

impl Comparison {
    /// Names a state the way the vanilla palette does: a default state by its block id alone,
    /// any other state with every property. Azalea's default states come from its own vanilla
    /// data, not Steel's.
    fn state_name(&mut self, state: BlockState) -> &str {
        self.state_names.entry(state).or_insert_with(|| {
            let block = Box::<dyn BlockTrait>::from(state);
            let name = format!("minecraft:{}", block.id());
            if state == BlockState::from(BlockKind::from(state)) {
                return name;
            }
            let mut properties: Vec<(String, String)> = block
                .property_map()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            canonical(&name, &mut properties)
        })
    }

    fn compare(&mut self, pos: (i32, i32), sent: &Chunk, vanilla: &VanillaChunk) {
        for (section_index, section) in sent.sections.iter().enumerate() {
            let section_min_y = OVERWORLD_MIN_Y + 16 * section_index as i32;
            for local_y in 0..16u8 {
                for z in 0..16u8 {
                    for x in 0..16u8 {
                        let y = section_min_y + i32::from(local_y);
                        let state =
                            section.get_block_state(ChunkSectionBlockPos { x, y: local_y, z });
                        let expected = vanilla.block(usize::from(x), y, usize::from(z)).to_owned();
                        let actual = self.state_name(state).to_owned();
                        if actual == expected {
                            continue;
                        }
                        if self.may_change.contains(&pos) {
                            self.tolerated_blocks += 1;
                            self.tolerated_chunks.insert(pos);
                            continue;
                        }
                        self.mismatched_blocks += 1;
                        self.mismatched_chunks.insert(pos);
                        if self.mismatched_blocks <= MAX_REPORTED_MISMATCHES {
                            let block_x = pos.0 * 16 + i32::from(x);
                            let block_z = pos.1 * 16 + i32::from(z);
                            let _ = writeln!(
                                self.report,
                                "  ({block_x}, {y}, {block_z}): vanilla {expected}, steel {actual}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "builds and launches the steel binary; run with --ignored"]
#[expect(
    clippy::print_stdout,
    reason = "the report is the test's output under --nocapture"
)]
async fn converted_full_chunks_match_vanilla_block_by_block() {
    let fixture = fixture_world();
    let vanilla = VanillaRegions::open(&fixture.join("dimensions/minecraft/overworld/region"));
    let expected: BTreeSet<(i32, i32)> = vanilla
        .chunk_statuses()
        .into_iter()
        .filter(|(_, status)| status == "minecraft:full")
        .map(|(pos, _)| pos)
        .collect();
    assert!(!expected.is_empty(), "fixture has no full chunks");

    let server_dir = fresh_dir("azalea_e2e");
    let report = convert(&ConvertOptions::new(&fixture, server_dir.join("saves")))
        .await
        .expect("conversion should succeed");
    println!("{report}");
    let level_toml = server_dir.join("saves/minecraft/worlds/overworld/level.toml");
    freeze_block_changing_gameplay(&level_toml);
    let may_change = chunks_vanilla_may_change(&vanilla, &expected, spawn_chunk(&level_toml));

    let server = SteelServer::start(
        &server_dir,
        &ServerSettings {
            view_distance: VIEW_DISTANCE,
            simulation_distance: SIMULATION_DISTANCE,
            worlds_toml: &report.worlds_toml("saves"),
        },
    )
    .await;

    // Azalea runs its ECS with `spawn_local`, so the bot must live inside a `LocalSet`.
    let comparison = Comparison {
        may_change,
        ..Comparison::default()
    };
    let (comparison, finished) = LocalSet::new()
        .run_until(receive_full_chunks(
            server.address(),
            &expected,
            &vanilla,
            comparison,
        ))
        .await;

    let log = server.log();
    let corrupt: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("Discarded corrupt chunk payload"))
        .collect();
    assert!(
        corrupt.is_empty(),
        "steel rejected {} converted chunks:\n{}",
        corrupt.len(),
        corrupt.join("\n")
    );

    let missing: Vec<_> = expected.difference(&comparison.received).collect();
    assert!(
        finished && missing.is_empty(),
        "received {}/{} full chunks before the timeout; first missing: {:?}",
        comparison.received.len(),
        expected.len(),
        missing.iter().take(10).collect::<Vec<_>>()
    );
    let strict = expected.len() - comparison.may_change.len();
    assert_eq!(
        comparison.mismatched_blocks,
        0,
        "{} blocks differ in {} of {strict} chunks vanilla would send as saved {:?}:\n{}",
        comparison.mismatched_blocks,
        comparison.mismatched_chunks.len(),
        comparison.mismatched_chunks,
        comparison.report
    );
    println!(
        "all {strict} full chunks that vanilla sends as saved match it block by block; \
         {} blocks differ in {} of the {} chunks vanilla may change first",
        comparison.tolerated_blocks,
        comparison.tolerated_chunks.len(),
        comparison.may_change.len()
    );
}

/// Joins Steel as an Azalea bot and compares each `expected` chunk as it arrives. Also returns
/// whether every expected chunk arrived before the timeout.
async fn receive_full_chunks(
    address: String,
    expected: &BTreeSet<(i32, i32)>,
    vanilla: &VanillaRegions,
    mut comparison: Comparison,
) -> (Comparison, bool) {
    let (client, mut events) = Client::join(Account::offline("converter_e2e"), address)
        .await
        .expect("resolve steel address");

    let receive = async {
        while let Some(event) = events.recv().await {
            match event {
                Event::Init => client
                    .set_client_information(ClientInformation {
                        view_distance: VIEW_DISTANCE,
                        ..ClientInformation::default()
                    })
                    .expect("set client information"),
                Event::Packet(packet) => {
                    let ClientboundGamePacket::LevelChunkWithLight(chunk) = &*packet else {
                        continue;
                    };
                    let pos = (chunk.x, chunk.z);
                    if !expected.contains(&pos) || !comparison.received.insert(pos) {
                        continue;
                    }
                    let sent = Chunk::read_with_dimension_height(
                        &mut Cursor::new(&chunk.chunk_data.buffer[..]),
                        OVERWORLD_HEIGHT,
                        OVERWORLD_MIN_Y,
                        &chunk.chunk_data.heightmaps,
                    )
                    .unwrap_or_else(|e| panic!("chunk {pos:?} packet failed to decode: {e}"));
                    let vanilla_chunk = vanilla
                        .read_chunk(pos.0, pos.1)
                        .expect("expected chunk is in the fixture");
                    comparison.compare(pos, &sent, &vanilla_chunk);
                    if comparison.received.len() == expected.len() {
                        return;
                    }
                }
                Event::Disconnect(reason) => panic!("bot disconnected: {reason:?}"),
                _ => {}
            }
        }
    };
    let finished = timeout(RECEIVE_TIMEOUT, receive).await.is_ok();
    client.disconnect();
    (comparison, finished)
}
